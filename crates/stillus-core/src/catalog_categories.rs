// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! Category changes preserve every item and its body; only category metadata changes.
use super::*;

fn in_category(value: &str, category: &str) -> bool {
    value == category
        || value
            .strip_prefix(category)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn renamed_category(value: &str, source: &str, target: Option<&str>) -> Option<String> {
    if in_category(value, source) {
        target.map(|target| format!("{target}{}", &value[source.len()..]))
    } else {
        Some(value.to_owned())
    }
}

fn changed_tags(
    tags: &[String],
    source: &str,
    target: Option<&str>,
) -> Result<Vec<String>, CoreError> {
    normalize_rss_categories(
        &tags
            .iter()
            .filter_map(|tag| renamed_category(tag, source, target))
            .collect::<Vec<_>>(),
    )
}

fn normalized_category(value: &str) -> Result<String, CoreError> {
    normalize_rss_categories(&[value.to_owned()])?
        .pop()
        .ok_or_else(|| CoreError::NoteUnavailable("category name is empty".into()))
}

fn changed_order(
    order: &BTreeMap<String, u32>,
    source: &str,
    target: Option<&str>,
) -> BTreeMap<String, u32> {
    let mut result = order
        .iter()
        .filter(|(key, _)| !in_category(key, source))
        .map(|(key, value)| (key.clone(), *value))
        .collect::<BTreeMap<_, _>>();
    for (key, value) in order.iter().filter(|(key, _)| in_category(key, source)) {
        if let Some(key) = renamed_category(key, source, target) {
            result.entry(key).or_insert(*value);
        }
    }
    result
}

pub(super) struct NoteMetadataChange {
    pub(super) index: usize,
    pub(super) path: PathBuf,
    pub(super) version: FileVersion,
    pub(super) patch: MetadataPatch,
    pub(super) frontmatter: FrontMatterScan,
    pub(super) protected: bool,
}

impl WorkspaceSession {
    /// Rename a category and its descendants without moving or opening any item.
    /// Existing destinations retain their ordering when category memberships merge.
    pub fn rename_category(
        &mut self,
        source: &str,
        target: &str,
        timestamp: &str,
    ) -> Result<usize, CoreError> {
        let target = normalized_category(target)?;
        self.change_category(source, Some(&target), timestamp)
    }

    /// Remove category membership, including descendants, while retaining the items.
    pub fn remove_category(&mut self, source: &str, timestamp: &str) -> Result<usize, CoreError> {
        self.change_category(source, None, timestamp)
    }

    fn change_category(
        &mut self,
        source: &str,
        target: Option<&str>,
        timestamp: &str,
    ) -> Result<usize, CoreError> {
        let _operation = stillus_platform::OperationLock::directory(&self.root)
            .map_err(|error| CoreError::Workspace(error.to_string()))?;
        self.ensure_workspace_action_ready()?;
        let source = normalized_category(source)?;
        if target == Some(source.as_str()) {
            return Ok(0);
        }
        let items = self.non_document_items();
        let categories = self.notes.iter().flat_map(|note| note.tags.iter()).chain(
            items
                .iter()
                .flat_map(|item| item.metadata.categories.iter()),
        );
        if let Some(target) = target {
            if in_category(target, &source) {
                return Err(CoreError::Engine(EngineError::Conflict));
            }
        }
        if !categories
            .clone()
            .any(|category| in_category(category, &source))
        {
            return Ok(0);
        }
        let recovery = self.recovery_store.scan();
        let protected_recovery = self.recovery_store.scan_protected();
        if !recovery.diagnostics.is_empty() || !protected_recovery.diagnostics.is_empty() {
            return Err(CoreError::UnsavedChanges);
        }
        // Validate every affected note before publishing the first metadata change.
        let mut notes = Vec::new();
        for (index, note) in self.notes.iter().enumerate() {
            if !note.tags.iter().any(|tag| in_category(tag, &source)) {
                continue;
            }
            let key = self.recovery_store.key_for_note(&note.path)?;
            if note.recovery_available
                || recovery.records.iter().any(|record| record.key == key)
                || self.recovery_store.protected_exists(&key)?
            {
                return Err(CoreError::UnsavedChanges);
            }
            if !note.availability.is_ready() {
                return Err(CoreError::NoteUnavailable(
                    "category member is unavailable".into(),
                ));
            }
            let (mut file, version) = open_versioned(&note.path)?;
            let frontmatter =
                scan_reader(&mut file).map_err(|error| CoreError::Workspace(error.to_string()))?;
            let FrontMatterStatus::Parsed(parsed) = &frontmatter.status else {
                return Err(CoreError::Save(SaveError::Conflict));
            };
            if parsed.metadata.tags != note.tags
                || parsed.metadata.order != note.order
                || parsed.metadata.encryption.is_some()
                    != (note.protection == NoteProtection::Protected)
                || self
                    .document
                    .as_ref()
                    .filter(|document| document.target == DocumentTarget::WorkspaceNote(index))
                    .is_some_and(|document| document.file_version != Some(version))
            {
                return Err(CoreError::Save(SaveError::Conflict));
            }
            let patch = MetadataPatch {
                tags: Some(changed_tags(&note.tags, &source, target)?),
                order: Some(changed_order(&note.order, &source, target)),
                modified: Some(timestamp.to_owned()),
                ..Default::default()
            };
            let rewrite = patch_front_matter(&frontmatter, &patch)
                .map_err(|error| CoreError::Save(SaveError::Patch(error)))?
                .ok_or_else(|| {
                    CoreError::Save(SaveError::InvalidTarget("empty category change".into()))
                })?;
            let next = scan_reader(Cursor::new(&rewrite.prefix))
                .map_err(|error| CoreError::Workspace(error.to_string()))?;
            notes.push(NoteMetadataChange {
                index,
                path: note.path.clone(),
                version,
                patch,
                frontmatter: next,
                protected: note.protection == NoteProtection::Protected,
            });
        }
        let engines = items
            .into_iter()
            .filter(|item| {
                item.metadata
                    .categories
                    .iter()
                    .any(|category| in_category(category, &source))
            })
            .map(|item| {
                let patch = stillus_engine::CommonMetadataPatch {
                    categories: Some(changed_tags(&item.metadata.categories, &source, target)?),
                    order: Some(changed_order(&item.metadata.order, &source, target)),
                    ..Default::default()
                };
                Ok((item, patch))
            })
            .collect::<Result<Vec<_>, CoreError>>()?;
        let mut changed = 0;
        // Each publication retains the storage layer's version checks and recovery guarantees.
        // An external conflict stops the remaining changes; a retry is safe and idempotent.
        for change in notes {
            self.apply_catalog_note_metadata(change)?;
            changed += 1;
        }
        for (item, patch) in engines {
            self.update_engine_metadata(
                &item.engine_id,
                &item.item_id,
                &item.metadata_version,
                patch,
            )?;
            changed += 1;
        }
        self.refresh_catalog_categories();
        Ok(changed)
    }

    pub(super) fn apply_catalog_note_metadata(
        &mut self,
        change: NoteMetadataChange,
    ) -> Result<(), CoreError> {
        let NoteMetadataChange {
            index,
            path,
            version,
            patch,
            frontmatter,
            protected,
        } = change;
        let commit = if protected {
            match rewrite_protected_metadata_versioned(&self.root, &path, &version, &patch, None)? {
                VerifiedSave::Verified(commit) => commit,
                VerifiedSave::IntegrityFailure(failure) => {
                    if let Some(document) = self
                        .document
                        .as_mut()
                        .filter(|document| document.target == DocumentTarget::WorkspaceNote(index))
                    {
                        document.file_version = Some(failure.commit.version);
                    }
                    self.pending_integrity = Some(PendingIntegrity {
                        failure,
                        retry: Some(IntegrityRetry::Metadata(Box::new(ProtectedMetadataJob {
                            note_index: index,
                            path,
                            version,
                            patch,
                            next_frontmatter: frontmatter,
                            workspace: self.root.clone(),
                            rename_title: None,
                        }))),
                    });
                    return Err(CoreError::UnsavedChanges);
                }
            }
        } else {
            rewrite_metadata_versioned(&path, &version, &patch)?
        };
        if let Some(document) = self
            .document
            .as_mut()
            .filter(|document| document.target == DocumentTarget::WorkspaceNote(index))
        {
            document.file_version = Some(commit.version);
            if let DocumentProtection::Protected(protected) = &mut document.protection {
                protected.frontmatter = frontmatter.clone();
            }
        }
        let note = &mut self.notes[index];
        if let Some(tags) = patch.tags {
            note.tags = tags;
        }
        if let Some(order) = patch.order {
            note.order = order;
        }
        if let Some(modified) = patch.modified {
            note.modified = Some(modified);
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
        note.body_offset = match frontmatter.status {
            FrontMatterStatus::Parsed(parsed) => Some(parsed.body_offset),
            _ => note.body_offset,
        };
        self.refresh_catalog_categories();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Home(PathBuf);
    impl Home {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "stillus-categories-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(root.join("notes")).unwrap();
            Self(root)
        }
        fn note(&self, name: &str, text: &str) -> PathBuf {
            let path = self.0.join("notes").join(name);
            fs::write(&path, text).unwrap();
            path
        }
    }
    impl Drop for Home {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn empty_or_reserved_category_actions_fail_without_changing_notes() {
        let home = Home::new();
        let path = home.note(
            "Alpha.md",
            "---\ntags: [Work]\ncustom: retained\n---\nAlpha\nBody\n",
        );
        let before = fs::read(&path).unwrap();
        let mut session = WorkspaceSession::open(&home.0).unwrap();
        for invalid in ["", "   ", "\t\n", FAVORITED_ORDER_KEY] {
            assert!(session.rename_category("Work", invalid, "now").is_err());
            assert!(session.rename_category(invalid, "Next", "now").is_err());
            assert!(session.remove_category(invalid, "now").is_err());
            assert_eq!(fs::read(&path).unwrap(), before);
            assert_eq!(session.notes()[0].tags, vec!["Work".to_owned()]);
        }
    }

    #[test]
    fn rename_and_remove_categories_preserve_notes_feeds_chats_and_selection() {
        let home = Home::new();
        let path = home.note("alpha.md", "---\ntitle: Alpha\ntags: [Work, 'Work/Child', Other]\norder: {Work: 2, 'Work/Child': 3, Other: 4}\ncustom: retained\n---\nAlpha\nBody\n");
        let unrelated = home.note("beta.md", "---\ntags: [Workshop]\n---\nBeta\n");
        let untouched = fs::read(&unrelated).unwrap();
        let chat_store = stillus_chat::ChatStore::open(&home.0).unwrap();
        let chat = chat_store
            .create_chat(stillus_chat::Metadata {
                common: stillus_engine::CommonMetadata {
                    title: "Chat".into(),
                    categories: vec!["Work/Child".into()],
                    ..Default::default()
                },
                automatic_title: false,
                alias: "default".into(),
            })
            .unwrap();
        let mut session = WorkspaceSession::open(&home.0).unwrap();
        let feed = session
            .create_rss(
                "https://example.test/feed",
                vec!["Work".into()],
                false,
                "now",
            )
            .unwrap();
        let index = session
            .notes
            .iter()
            .position(|note| note.path == path)
            .unwrap();
        session.open_note(index).unwrap();
        let selection = session.document().unwrap().selection();
        assert_eq!(
            session
                .rename_category("Work", "Projects", "later")
                .unwrap(),
            3
        );
        assert_eq!(session.document().unwrap().selection(), selection);
        assert_eq!(session.selected_note(), Some(index));
        assert_eq!(
            session.notes[index].tags,
            ["Projects", "Projects/Child", "Other"]
        );
        assert_eq!(session.notes[index].order["Projects/Child"], 3);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("custom: retained"));
        assert!(text.ends_with("Alpha\nBody\n"));
        assert_eq!(
            chat_store.metadata(&chat).unwrap().value.common.categories,
            ["Projects/Child"]
        );
        assert_eq!(session.remove_category("Projects", "latest").unwrap(), 3);
        assert_eq!(session.notes[index].tags, ["Other"]);
        assert_eq!(
            session.notes[index].order,
            BTreeMap::from([("Other".into(), 4)])
        );
        assert!(path.exists());
        assert_eq!(fs::read(&unrelated).unwrap(), untouched);
        assert!(
            session
                .rss_engine
                .subscriptions()
                .iter()
                .any(|item| item.id == feed && !item.deleted)
        );
        assert!(
            chat_store
                .metadata(&chat)
                .unwrap()
                .value
                .common
                .categories
                .is_empty()
        );
        assert_eq!(session.remove_category("Projects", "again").unwrap(), 0);
    }

    #[test]
    fn stale_category_metadata_and_recovery_prevent_partial_changes() {
        let home = Home::new();
        let first = home.note("alpha.md", "---\ntags: [Work]\n---\nAlpha\n");
        let second = home.note("beta.md", "---\ntags: [Work]\n---\nBeta\n");
        let mut session = WorkspaceSession::open(&home.0).unwrap();
        let original = fs::read(&first).unwrap();
        fs::write(&second, "---\ntags: [Work, External]\n---\nBeta\n").unwrap();
        assert!(matches!(
            session.rename_category("Work", "Next", "now"),
            Err(CoreError::Save(SaveError::Conflict))
        ));
        assert_eq!(fs::read(&first).unwrap(), original);
        let mut session = WorkspaceSession::open(&home.0).unwrap();
        session
            .notes
            .iter_mut()
            .find(|note| note.path == second)
            .unwrap()
            .recovery_available = true;
        assert!(matches!(
            session.remove_category("Work", "now"),
            Err(CoreError::UnsavedChanges)
        ));
        assert_eq!(fs::read(&first).unwrap(), original);
    }

    #[test]
    fn protected_category_changes_keep_the_authenticated_envelope_byte_identical() {
        let home = Home::new();
        let path = home.note(
            "secret.md",
            "---\ntitle: Secret\ntags: ['Work/Secret']\ncustom: kept\n---\nConfidential body\n",
        );
        let password = MasterPassword::new("category metadata password".into());
        let (_, version) = open_versioned(&path).unwrap();
        protect_note_body(&path, &version, &password, "Secret").unwrap();
        let before = fs::read(&path).unwrap();
        let envelope = |bytes: &[u8]| {
            let start = bytes
                .windows(stillus_secure::ARMORED_AGE_PREFIX.len())
                .position(|part| part == stillus_secure::ARMORED_AGE_PREFIX)
                .unwrap();
            bytes[start..].to_vec()
        };
        let mut session = WorkspaceSession::open(&home.0).unwrap();
        assert_eq!(
            session.rename_category("Work", "Private", "later").unwrap(),
            1
        );
        let after = fs::read(&path).unwrap();
        assert_eq!(envelope(&before), envelope(&after));
        assert!(!String::from_utf8_lossy(&after).contains("Confidential body"));
        assert_eq!(session.notes[0].tags, ["Private/Secret"]);
        assert!(session.document().is_none());
    }

    #[test]
    fn category_merge_preserves_existing_order_and_prefix_boundaries() {
        assert_eq!(
            changed_tags(
                &["Work".into(), "Next".into(), "Workshop".into()],
                "Work",
                Some("Next")
            )
            .unwrap(),
            ["Next", "Workshop"]
        );
        assert_eq!(
            changed_order(
                &BTreeMap::from([("Work".into(), 2), ("Next".into(), 7)]),
                "Work",
                Some("Next")
            ),
            BTreeMap::from([("Next".into(), 7)])
        );
    }
}
