// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::mpsc::SyncSender;
use stillus_core::{SecureJob, SecureWorkerEvent};
use stillus_secure::MasterPassword;

pub(crate) enum PendingSecurityAction {
    Protect {
        note_path: PathBuf,
        password: Option<MasterPassword>,
    },
    Lock {
        note_path: PathBuf,
    },
    DisableProtection {
        note_path: PathBuf,
    },
}

pub(crate) enum SearchSecurityOperation {
    Purging {
        operation_id: u64,
    },
    Restoring {
        operation_id: u64,
        completion: RestoreCompletion,
    },
}

pub(crate) enum SecureUiOperation {
    Unlock {
        restore_recovery: bool,
    },
    OpenProtected,
    Protect {
        action: PendingSecurityAction,
        note_path: PathBuf,
    },
    DisableProtection,
    Metadata,
    ExternalPoll,
    DiscardReload,
    RestoreRecovery,
    Integrity,
    ChangeMasterPassword,
}

pub(crate) enum RestoreCompletion {
    Protected,
    PurgeFailed(String),
    RetryProtect(PendingSecurityAction),
    AuthenticationFailed,
    ProtectFailed,
}

pub(crate) enum PendingPasswordChangeState {
    WaitingPersistence,
    WaitingSearch { operation_id: u64 },
}

pub(crate) struct PendingPasswordChange {
    pub(crate) current: MasterPassword,
    pub(crate) new: MasterPassword,
    pub(crate) state: PendingPasswordChangeState,
}

impl PendingSecurityAction {
    pub(crate) fn note_path(&self) -> &Path {
        match self {
            Self::Protect { note_path, .. }
            | Self::Lock { note_path }
            | Self::DisableProtection { note_path } => note_path,
        }
    }

    pub(crate) fn replace_note_path(&mut self, old_path: &Path, new_path: &Path) {
        match self {
            Self::Protect { note_path, .. }
            | Self::Lock { note_path }
            | Self::DisableProtection { note_path }
                if note_path == old_path =>
            {
                *note_path = new_path.to_path_buf();
            }
            _ => {}
        }
    }

    #[cfg(test)]
    pub(crate) fn has_password(&self) -> bool {
        matches!(
            self,
            Self::Protect {
                password: Some(_),
                ..
            }
        )
    }
}

pub(crate) fn start(job: SecureJob, sender: SyncSender<SecureWorkerEvent>) {
    std::thread::spawn(move || {
        let completion = job.execute_with_progress(|progress| {
            let _ = sender.try_send(SecureWorkerEvent::Progress(progress));
        });
        let _ = sender.send(SecureWorkerEvent::Completed(Box::new(completion)));
    });
}
