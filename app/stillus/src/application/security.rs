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
        #[cfg(feature = "test-utils")]
        if let Some(marker) = std::env::var_os("STILLUS_TEST_SECURE_GATE") {
            assert!(
                wait_for_test_release(Path::new(&marker), std::time::Duration::from_secs(30))
                    .is_ok(),
                "secure acceptance gate failed"
            );
        }
        let completion = job.execute_with_progress(|progress| {
            let _ = sender.try_send(SecureWorkerEvent::Progress(progress));
        });
        let _ = sender.send(SecureWorkerEvent::Completed(Box::new(completion)));
    });
}

/// The acceptance driver removes an empty marker after inspecting the busy UI.
/// Only test builds can delay the worker; the marker never contains a secret.
#[cfg(any(test, feature = "test-utils"))]
fn wait_for_test_release(path: &Path, timeout: std::time::Duration) -> std::io::Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
            Ok(metadata) if metadata.is_file() && metadata.len() == 0 => {}
            Ok(_) => return Err(std::io::Error::other("invalid secure acceptance marker")),
        }
        if std::time::Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "secure acceptance marker was not released",
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn acceptance_gate_waits_until_the_driver_releases_it() {
        let root = crate::test_support::workspace("stillus-secure-gate");
        let marker = root.join("pending");
        std::fs::write(&marker, b"").unwrap();
        let waiting = marker.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            sender
                .send(wait_for_test_release(&waiting, Duration::from_secs(5)))
                .unwrap();
        });
        assert!(receiver.recv_timeout(Duration::from_millis(50)).is_err());
        std::fs::remove_file(&marker).unwrap();
        receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        worker.join().unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn acceptance_gate_rejects_invalid_markers_and_bounds_waiting() {
        let root = crate::test_support::workspace("stillus-secure-gate-invalid");
        let marker = root.join("pending");
        wait_for_test_release(&marker, Duration::ZERO).unwrap();
        std::fs::write(&marker, b"").unwrap();
        assert_eq!(
            wait_for_test_release(&marker, Duration::ZERO)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::TimedOut
        );
        std::fs::write(&marker, b"invalid").unwrap();
        assert!(wait_for_test_release(&marker, Duration::ZERO).is_err());
        assert_eq!(std::fs::read(&marker).unwrap(), b"invalid");
        assert!(wait_for_test_release(&root, Duration::ZERO).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
