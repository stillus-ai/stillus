// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! Fixed-vocabulary deletion diagnostics; never format an error payload.

use super::CoreError;

pub(super) fn result<T>(
    _enabled: bool,
    _stage: &'static str,
    result: Result<T, CoreError>,
) -> Result<T, CoreError> {
    #[cfg(any(test, feature = "test-utils"))]
    if _enabled {
        let (outcome, error, error_stage) = match &result {
            Ok(_) => ("Success", "None".to_owned(), "None".to_owned()),
            Err(error) => {
                let (kind, stage) = classify(error);
                ("Failed", kind, stage)
            }
        };
        eprintln!(
            "NATIVE_DELETE stage={_stage} outcome={outcome} error={error} error_stage={error_stage}"
        );
    }
    result
}

#[cfg(any(test, feature = "test-utils"))]
fn classify(error: &CoreError) -> (String, String) {
    use stillus_recovery::RecoveryError;
    use stillus_storage::{NoteOperationError, SaveError};

    let kind = match error {
        CoreError::Save(save) | CoreError::Operation(NoteOperationError::Save(save)) => {
            let stage = if let SaveError::PreCommit { stage, .. } = save {
                format!("{stage:?}")
            } else {
                "None".to_owned()
            };
            let kind = match save {
                SaveError::UnsupportedPlatform => "Save/UnsupportedPlatform",
                SaveError::InvalidTarget(_) => "Save/InvalidTarget",
                SaveError::Patch(_) => "Save/Patch",
                SaveError::Conflict => "Save/Conflict",
                SaveError::PreCommit { .. } => "Save/PreCommit",
                SaveError::PostReplaceSync { .. } => "Save/PostReplaceSync",
                SaveError::PartialCommit { .. } => "Save/PartialCommit",
            };
            return (
                if matches!(error, CoreError::Operation(_)) {
                    format!("Operation/{kind}")
                } else {
                    kind.to_owned()
                },
                stage,
            );
        }
        CoreError::Recovery(error) => match error {
            RecoveryError::UnsupportedPlatform => "Recovery/UnsupportedPlatform",
            RecoveryError::InvalidPath(_) => "Recovery/InvalidPath",
            RecoveryError::InvalidStore(_) => "Recovery/InvalidStore",
            RecoveryError::InvalidArtifact(_) => "Recovery/InvalidArtifact",
            RecoveryError::Io(_) => "Recovery/Io",
        },
        CoreError::Operation(error) => match error {
            NoteOperationError::InvalidName(_) => "Operation/InvalidName",
            NoteOperationError::InvalidTag(_) => "Operation/InvalidTag",
            NoteOperationError::InvalidWorkspace(_) => "Operation/InvalidWorkspace",
            NoteOperationError::Collision(_) => "Operation/Collision",
            NoteOperationError::Conflict => "Operation/Conflict",
            NoteOperationError::Failed { stage, .. } => {
                return ("Operation/Failed".to_owned(), format!("{stage:?}"));
            }
            NoteOperationError::PartialCommit { .. } => "Operation/PartialCommit",
            NoteOperationError::Save(_) => unreachable!(),
        },
        CoreError::Workspace(_) => "Workspace",
        CoreError::NoteUnavailable(_) => "NoteUnavailable",
        CoreError::UnsavedChanges => "UnsavedChanges",
        CoreError::Secure(_) => "Secure",
        CoreError::Security(_) => "Security",
        CoreError::PasswordChange(_) => "PasswordChange",
        CoreError::MasterPasswordRequired => "MasterPasswordRequired",
        CoreError::Clock(_) => "Clock",
        CoreError::Editor(_) => "Editor",
        CoreError::Engine(_) => "Engine",
    };
    (kind.to_owned(), "None".to_owned())
}
