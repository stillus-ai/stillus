// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use super::*;

/// Removes only a version-matched, explicitly trashed canonical note. The bounded
/// front-matter scan never decrypts or copies its body, and no recovery is removed.
pub fn delete_trashed_note_versioned(
    workspace: &Path,
    source: &Path,
    expected: &FileVersion,
) -> Result<(), NoteOperationError> {
    let _operation = stillus_platform::OperationLock::directory(workspace)
        .map_err(|error| SaveError::InvalidTarget(error.to_string()))?;
    delete_with(workspace, source, expected, &mut NoOperationFault)
}

fn delete_with(
    workspace: &Path,
    source: &Path,
    expected: &FileVersion,
    checkpoint: &mut impl OperationCheckpoint,
) -> Result<(), NoteOperationError> {
    let notes = direct_notes_directory(workspace)?;
    validate_direct_note(source, &notes)?;
    let (mut file, actual) = open_versioned(source)?;
    if actual != *expected {
        return Err(NoteOperationError::Conflict);
    }
    let scan = scan_reader(&mut file)
        .map_err(|error| operation_failure(OperationStage::Validate, error))?;
    if !matches!(scan.status, FrontMatterStatus::Parsed(ref parsed) if parsed.metadata.deleted == Some(true))
    {
        return Err(NoteOperationError::InvalidWorkspace(
            "permanent deletion requires a trashed note".into(),
        ));
    }
    drop(file);
    checkpoint.check(OperationStage::SourceRemove)?;
    // Revalidate after the front-matter read and any delayed confirmation.
    validate_direct_note(source, &direct_notes_directory(workspace)?)?;
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| operation_failure(OperationStage::SourceRemove, error))?;
    if FileVersion::from_metadata(&metadata) != *expected {
        return Err(NoteOperationError::Conflict);
    }
    fs::remove_file(source)
        .map_err(|error| operation_failure(OperationStage::SourceRemove, error))?;
    sync_directory(&notes, checkpoint).map_err(|error| NoteOperationError::PartialCommit {
        message: format!("note was deleted, but its directory could not be synced: {error}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "stillus-permanent-delete-{}-{}",
                std::process::id(),
                TEMP_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(path.join("notes")).unwrap();
            Self(path)
        }
        fn note(&self, contents: &str) -> (PathBuf, FileVersion) {
            let path = self.0.join("notes/Note.md");
            fs::write(&path, contents).unwrap();
            let version = open_versioned(&path).unwrap().1;
            (path, version)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn permanent_delete_removes_only_the_confirmed_trashed_note() {
        let fixture = Fixture::new();
        let (path, version) = fixture.note("---\ndeleted: true\n---\nbody\n");
        for directory in [".stillus/recovery", ".stillus_security", ".stillus_backups"] {
            fs::create_dir_all(fixture.0.join(directory)).unwrap();
            fs::write(fixture.0.join(directory).join("keep"), "unrelated").unwrap();
        }
        delete_trashed_note_versioned(&fixture.0, &path, &version).unwrap();
        assert!(!path.exists());
        for directory in [".stillus/recovery", ".stillus_security", ".stillus_backups"] {
            assert_eq!(
                fs::read_to_string(fixture.0.join(directory).join("keep")).unwrap(),
                "unrelated"
            );
        }
    }

    #[test]
    fn permanent_delete_rejects_active_notes_and_stale_confirmation() {
        let fixture = Fixture::new();
        let (path, version) = fixture.note("---\ndeleted: false\n---\nbody\n");
        assert!(delete_trashed_note_versioned(&fixture.0, &path, &version).is_err());
        let (path, version) = fixture.note("---\ndeleted: true\n---\nbody\n");
        fs::write(&path, "---\ndeleted: true\n---\nexternal edit\n").unwrap();
        assert_eq!(
            delete_trashed_note_versioned(&fixture.0, &path, &version),
            Err(NoteOperationError::Conflict)
        );
        assert!(fs::read_to_string(path).unwrap().contains("external edit"));
    }

    #[test]
    fn permanent_delete_checks_version_again_immediately_before_removal() {
        struct Replace(PathBuf);
        impl OperationCheckpoint for Replace {
            fn check(&mut self, stage: OperationStage) -> Result<(), NoteOperationError> {
                if stage == OperationStage::SourceRemove {
                    fs::write(
                        &self.0,
                        "---\ndeleted: true\n---\nchanged after validation\n",
                    )
                    .unwrap();
                }
                Ok(())
            }
        }
        let fixture = Fixture::new();
        let (path, version) = fixture.note("---\ndeleted: true\n---\nbody\n");
        assert_eq!(
            delete_with(&fixture.0, &path, &version, &mut Replace(path.clone())),
            Err(NoteOperationError::Conflict)
        );
        assert!(path.exists());
    }

    #[test]
    fn permanent_delete_does_not_read_a_large_encrypted_body() {
        let fixture = Fixture::new();
        let (path, _) = fixture.note(
            "---\ndeleted: true\nstillus_encryption: age-body-v1\n---\nage-encryption.org/v1\n",
        );
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(128 * 1024 * 1024)
            .unwrap();
        let version = open_versioned(&path).unwrap().1;
        delete_trashed_note_versioned(&fixture.0, &path, &version).unwrap();
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn permanent_delete_rejects_symlinks_and_foreign_paths() {
        let fixture = Fixture::new();
        let (path, version) = fixture.note("---\ndeleted: true\n---\nbody\n");
        let link = fixture.0.join("notes/Link.md");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(delete_trashed_note_versioned(&fixture.0, &link, &version).is_err());
        let foreign = fixture.0.join("Outside.md");
        fs::write(&foreign, "---\ndeleted: true\n---\nbody\n").unwrap();
        let version = open_versioned(&foreign).unwrap().1;
        assert!(delete_trashed_note_versioned(&fixture.0, &foreign, &version).is_err());
        assert!(foreign.exists() && path.exists());
    }
}
