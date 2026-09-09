// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use stillus_platform::fs::{self, File, OpenOptions};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use stillus_frontmatter::{EncryptionFormat, FrontMatterStatus, scan_reader};
use stillus_secure::{
    ArmoredEnvelopeWriter, BodyEnvelopeWriter, EnvelopeKind, EnvelopeMetadata, EnvelopeWriter,
    MasterPassword, decrypt, decrypt_armored, decrypt_body,
};

use crate::secure_backups::{ensure_store, finalize_verified, prepare_backup};
use crate::{FileVersion, SaveError, load_pending_integrity_failure, open_versioned};

const JOURNAL_VERSION: u32 = 1;
fn legacy_journal_platform() -> String {
    "unix".to_owned()
}
fn journal_platform() -> &'static str {
    if cfg!(windows) { "windows" } else { "unix" }
}

const BUFFER_BYTES: usize = 64 * 1024;
// Candidate preparation performs the expensive decrypt/encrypt/authenticate
// passes. Ciphertext backup, atomic installation and final hashing are much
// cheaper, so an equal-per-event scale would leave most of the percentage for
// work that finishes almost immediately.
const PREPARATION_WORK_UNITS: usize = 12;
const BACKUP_WORK_UNITS: usize = 1;
const INSTALL_WORK_UNITS: usize = 1;
const VERIFY_WORK_UNITS: usize = 1;
static TRANSACTION_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PasswordChangePhase {
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
pub struct PasswordChangeProgress {
    pub phase: PasswordChangePhase,
    pub completed: usize,
    pub total: usize,
    pub percent: Option<u8>,
}

fn forward_percent(completed: usize, total: usize) -> u8 {
    if total == 0 {
        return 0;
    }
    (((completed as u128) * 100 / (total as u128)).min(99)) as u8
}

fn report_forward_step(
    progress: &mut impl FnMut(PasswordChangeProgress),
    completed_work: &mut usize,
    total_work: usize,
    work_units: usize,
    phase: PasswordChangePhase,
    completed: usize,
    total: usize,
) {
    *completed_work = completed_work.saturating_add(work_units);
    progress(PasswordChangeProgress {
        phase,
        completed,
        total,
        percent: Some(forward_percent(*completed_work, total_work)),
    });
}

#[derive(Clone, Debug)]
pub struct PasswordChangeTarget {
    pub path: PathBuf,
    pub version: FileVersion,
}

#[derive(Clone, Debug)]
pub struct PasswordChangeCommit {
    pub note_versions: Vec<(PathBuf, FileVersion)>,
    pub note_count: usize,
    pub recovery_count: usize,
    pub secret_count: usize,
}

pub type SecurityRotationCommit = PasswordChangeCommit;
pub type SecurityRotationError = PasswordChangeError;
pub type SecurityRotationPhase = PasswordChangePhase;
pub type SecurityRotationProgress = PasswordChangeProgress;
pub type SecurityRotationTarget = PasswordChangeTarget;

#[derive(Clone, Copy)]
pub struct SecurityRotationTargets<'a> {
    pub verifier: Option<&'a PasswordChangeTarget>,
    pub secrets: &'a [PasswordChangeTarget],
    pub notes: &'a [PasswordChangeTarget],
    pub recovery: &'a [PathBuf],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PasswordChangeError {
    Invalid(String),
    Conflict(String),
    RolledBack(String),
    Blocked(String),
    Io(String),
}

impl PasswordChangeError {
    pub fn blocks_workspace(&self) -> bool {
        matches!(self, Self::Blocked(_))
    }
}

impl std::fmt::Display for PasswordChangeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) => write!(formatter, "password change rejected: {message}"),
            Self::Conflict(message) => write!(formatter, "password change conflict: {message}"),
            Self::RolledBack(message) => {
                write!(
                    formatter,
                    "password change failed and was rolled back: {message}"
                )
            }
            Self::Blocked(message) => write!(
                formatter,
                "password change recovery requires attention: {message}"
            ),
            Self::Io(message) => write!(formatter, "password change I/O failed: {message}"),
        }
    }
}

impl std::error::Error for PasswordChangeError {}

#[derive(Clone)]
struct Snapshot {
    kind: TargetKind,
    path: PathBuf,
    version: FileVersion,
    old_sha256: String,
    plaintext_sha256: String,
    plaintext_len: u64,
    mode: u32,
    #[cfg(windows)]
    permissions: fs::Permissions,
    recorded_version: RecordedFileVersion,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct RecordedFileVersion {
    size: u64,
    #[cfg(any(unix, windows))]
    device: u64,
    #[cfg(any(unix, windows))]
    inode: u64,
    #[cfg(any(unix, windows))]
    modified_seconds: i64,
    #[cfg(any(unix, windows))]
    modified_nanoseconds: i64,
    #[cfg(any(unix, windows))]
    changed_seconds: i64,
    #[cfg(any(unix, windows))]
    changed_nanoseconds: i64,
    #[cfg(windows)]
    digest: [u8; 32],
}

impl RecordedFileVersion {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        Self {
            #[cfg(windows)]
            digest: metadata.digest(),
            size: metadata.len(),
            #[cfg(any(unix, windows))]
            device: metadata.dev(),
            #[cfg(any(unix, windows))]
            inode: metadata.ino(),
            #[cfg(any(unix, windows))]
            modified_seconds: metadata.mtime(),
            #[cfg(any(unix, windows))]
            modified_nanoseconds: metadata.mtime_nsec(),
            #[cfg(any(unix, windows))]
            changed_seconds: metadata.ctime(),
            #[cfg(any(unix, windows))]
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }

    fn matches(&self, metadata: &fs::Metadata) -> bool {
        self == &Self::from_metadata(metadata)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TargetKind {
    Verifier,
    EngineSecret,
    Note,
    Recovery,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TransactionState {
    Preparing,
    Commit,
    Rollback,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Journal {
    #[serde(default = "legacy_journal_platform")]
    platform: String,
    version: u32,
    state: TransactionState,
    entries: Vec<JournalEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct JournalEntry {
    kind: TargetKind,
    source: String,
    rollback_copy: String,
    candidate: String,
    old_sha256: String,
    new_sha256: String,
    old_version: RecordedFileVersion,
    mode: u32,
    #[cfg(windows)]
    permissions: fs::Permissions,
    installed: bool,
}

struct Transaction {
    workspace: PathBuf,
    directory: PathBuf,
    journal_path: PathBuf,
    journal: Journal,
}

pub fn change_master_password(
    workspace: impl AsRef<Path>,
    notes: &[PasswordChangeTarget],
    recovery_paths: &[PathBuf],
    current: &MasterPassword,
    new: &MasterPassword,
    progress: impl FnMut(PasswordChangeProgress),
) -> Result<PasswordChangeCommit, PasswordChangeError> {
    rotate_workspace_security(
        workspace,
        SecurityRotationTargets {
            verifier: None,
            secrets: &[],
            notes,
            recovery: recovery_paths,
        },
        current,
        new,
        progress,
    )
}

pub fn rotate_workspace_security(
    workspace: impl AsRef<Path>,
    targets: SecurityRotationTargets<'_>,
    current: &MasterPassword,
    new: &MasterPassword,
    mut progress: impl FnMut(PasswordChangeProgress),
) -> Result<PasswordChangeCommit, PasswordChangeError> {
    let _operation = stillus_platform::OperationLock::directory(workspace.as_ref())
        .map_err(|error| PasswordChangeError::Blocked(error.to_string()))?;
    let SecurityRotationTargets {
        verifier,
        secrets,
        notes,
        recovery: recovery_paths,
    } = targets;
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (workspace, targets, current, new, progress);
        return Err(PasswordChangeError::Invalid(
            "password change is supported on Unix and Windows".to_owned(),
        ));
    }

    #[cfg(any(unix, windows))]
    {
        let workspace = workspace.as_ref();
        if current.is_empty() || new.is_empty() {
            return Err(PasswordChangeError::Invalid(
                "current and new passwords are required".to_owned(),
            ));
        }
        if current.same_secret(new) {
            return Err(PasswordChangeError::Invalid(
                "new password must differ from the current password".to_owned(),
            ));
        }
        reject_active_transaction(workspace)?;
        if load_pending_integrity_failure(workspace)
            .map_err(save_error)?
            .is_some()
        {
            return Err(PasswordChangeError::Invalid(
                "workspace has an unresolved integrity incident".to_owned(),
            ));
        }
        if verifier.is_none() && secrets.is_empty() && notes.is_empty() && recovery_paths.is_empty()
        {
            return Err(PasswordChangeError::Invalid(
                "workspace has no encrypted targets".to_owned(),
            ));
        }

        // Authentication and structural checks happen before the first write.
        let total =
            usize::from(verifier.is_some()) + secrets.len() + notes.len() + recovery_paths.len();
        let mut snapshots = Vec::with_capacity(total);
        let mut unique_targets = HashSet::with_capacity(total);
        if let Some(target) = verifier {
            if !unique_targets.insert(target.path.clone()) {
                return Err(PasswordChangeError::Invalid(
                    "password change target is duplicated".to_owned(),
                ));
            }
            snapshots.push(preflight_armored(
                workspace,
                target,
                current,
                TargetKind::Verifier,
                EnvelopeKind::WorkspaceVerifier,
            )?);
            progress(PasswordChangeProgress {
                phase: PasswordChangePhase::Validating,
                completed: snapshots.len(),
                total,
                percent: Some(0),
            });
        }
        for target in secrets {
            if !unique_targets.insert(target.path.clone()) {
                return Err(PasswordChangeError::Invalid(
                    "password change target is duplicated".to_owned(),
                ));
            }
            snapshots.push(preflight_armored(
                workspace,
                target,
                current,
                TargetKind::EngineSecret,
                EnvelopeKind::EngineSecret,
            )?);
            progress(PasswordChangeProgress {
                phase: PasswordChangePhase::Validating,
                completed: snapshots.len(),
                total,
                percent: Some(0),
            });
        }
        for target in notes {
            if !unique_targets.insert(target.path.clone()) {
                return Err(PasswordChangeError::Invalid(
                    "password change target is duplicated".to_owned(),
                ));
            }
            snapshots.push(preflight_note(workspace, target, current)?);
            progress(PasswordChangeProgress {
                phase: PasswordChangePhase::Validating,
                completed: snapshots.len(),
                total,
                percent: Some(0),
            });
        }
        for path in recovery_paths {
            if !unique_targets.insert(path.clone()) {
                return Err(PasswordChangeError::Invalid(
                    "password change target is duplicated".to_owned(),
                ));
            }
            snapshots.push(preflight_recovery(workspace, path, current)?);
            progress(PasswordChangeProgress {
                phase: PasswordChangePhase::Validating,
                completed: snapshots.len(),
                total,
                percent: Some(0),
            });
        }

        let mut transaction = Transaction::create(workspace)?;
        let result: Result<PasswordChangeCommit, PasswordChangeError> = (|| {
            let backup_targets = notes.len().saturating_add(secrets.len());
            let per_target_work = PREPARATION_WORK_UNITS
                .saturating_add(INSTALL_WORK_UNITS)
                .saturating_add(VERIFY_WORK_UNITS);
            let total_work = total
                .saturating_mul(per_target_work)
                .saturating_add(backup_targets.saturating_mul(BACKUP_WORK_UNITS));
            let mut completed_work = 0usize;
            let first_snapshot = snapshots.first().expect("encrypted targets are not empty");
            let (first_phase, first_total) = match first_snapshot.kind {
                TargetKind::Verifier => (PasswordChangePhase::PreparingVerifier, 1),
                TargetKind::EngineSecret => (PasswordChangePhase::PreparingSecrets, secrets.len()),
                TargetKind::Note => (PasswordChangePhase::PreparingNotes, notes.len()),
                TargetKind::Recovery => {
                    (PasswordChangePhase::PreparingRecovery, recovery_paths.len())
                }
            };
            progress(PasswordChangeProgress {
                phase: first_phase,
                completed: 0,
                total: first_total,
                percent: Some(0),
            });

            let mut verifier_completed = 0;
            let mut note_completed = 0;
            let mut recovery_completed = 0;
            let mut secret_completed = 0;
            for snapshot in &snapshots {
                transaction.prepare(snapshot, current, new)?;
                match snapshot.kind {
                    TargetKind::Verifier => {
                        verifier_completed += 1;
                        report_forward_step(
                            &mut progress,
                            &mut completed_work,
                            total_work,
                            PREPARATION_WORK_UNITS,
                            PasswordChangePhase::PreparingVerifier,
                            verifier_completed,
                            1,
                        );
                    }
                    TargetKind::EngineSecret => {
                        secret_completed += 1;
                        report_forward_step(
                            &mut progress,
                            &mut completed_work,
                            total_work,
                            PREPARATION_WORK_UNITS,
                            PasswordChangePhase::PreparingSecrets,
                            secret_completed,
                            secrets.len(),
                        );
                    }
                    TargetKind::Note => {
                        note_completed += 1;
                        report_forward_step(
                            &mut progress,
                            &mut completed_work,
                            total_work,
                            PREPARATION_WORK_UNITS,
                            PasswordChangePhase::PreparingNotes,
                            note_completed,
                            notes.len(),
                        );
                    }
                    TargetKind::Recovery => {
                        recovery_completed += 1;
                        report_forward_step(
                            &mut progress,
                            &mut completed_work,
                            total_work,
                            PREPARATION_WORK_UNITS,
                            PasswordChangePhase::PreparingRecovery,
                            recovery_completed,
                            recovery_paths.len(),
                        );
                    }
                }
            }

            for (completed, snapshot) in snapshots
                .iter()
                .filter(|snapshot| snapshot.kind == TargetKind::Note)
                .enumerate()
            {
                let backup = prepare_backup(workspace, &snapshot.path, &snapshot.version)
                    .map_err(save_error)?;
                finalize_verified(workspace, &backup, &snapshot.path).map_err(save_error)?;
                report_forward_step(
                    &mut progress,
                    &mut completed_work,
                    total_work,
                    BACKUP_WORK_UNITS,
                    PasswordChangePhase::BackingUpNotes,
                    completed + 1,
                    notes.len(),
                );
            }

            for (completed, snapshot) in snapshots
                .iter()
                .filter(|snapshot| snapshot.kind == TargetKind::EngineSecret)
                .enumerate()
            {
                let backup = prepare_backup(workspace, &snapshot.path, &snapshot.version)
                    .map_err(save_error)?;
                finalize_verified(workspace, &backup, &snapshot.path).map_err(save_error)?;
                report_forward_step(
                    &mut progress,
                    &mut completed_work,
                    total_work,
                    BACKUP_WORK_UNITS,
                    PasswordChangePhase::BackingUpSecrets,
                    completed + 1,
                    secrets.len(),
                );
            }

            transaction.journal.state = TransactionState::Commit;
            transaction.save_journal()?;
            let mut note_versions = Vec::with_capacity(notes.len());
            for kind in [
                TargetKind::Recovery,
                TargetKind::EngineSecret,
                TargetKind::Note,
                TargetKind::Verifier,
            ] {
                let phase = match kind {
                    TargetKind::Recovery => PasswordChangePhase::ReplacingRecovery,
                    TargetKind::EngineSecret => PasswordChangePhase::ReplacingSecrets,
                    TargetKind::Note => PasswordChangePhase::ReplacingNotes,
                    TargetKind::Verifier => PasswordChangePhase::ReplacingVerifier,
                };
                let phase_total = transaction
                    .journal
                    .entries
                    .iter()
                    .filter(|entry| entry.kind == kind)
                    .count();
                let indices = transaction
                    .journal
                    .entries
                    .iter()
                    .enumerate()
                    .filter_map(|(index, entry)| (entry.kind == kind).then_some(index))
                    .collect::<Vec<_>>();
                for (completed, index) in indices.into_iter().enumerate() {
                    let (path, version) = transaction.install(index)?;
                    if kind == TargetKind::Note {
                        note_versions.push((path, version));
                    }
                    report_forward_step(
                        &mut progress,
                        &mut completed_work,
                        total_work,
                        INSTALL_WORK_UNITS,
                        phase,
                        completed + 1,
                        phase_total,
                    );
                }
            }
            for completed in 0..transaction.journal.entries.len() {
                transaction.verify_new(completed)?;
                report_forward_step(
                    &mut progress,
                    &mut completed_work,
                    total_work,
                    VERIFY_WORK_UNITS,
                    PasswordChangePhase::Verifying,
                    completed + 1,
                    total,
                );
            }
            transaction.cleanup()?;
            Ok(PasswordChangeCommit {
                note_versions,
                note_count: notes.len(),
                recovery_count: recovery_paths.len(),
                secret_count: secrets.len(),
            })
        })();

        match result {
            Ok(commit) => Ok(commit),
            Err(error) => {
                let rollback = transaction.rollback(&mut progress);
                match rollback {
                    Ok(()) => Err(PasswordChangeError::RolledBack(error.to_string())),
                    Err(rollback_error) => Err(PasswordChangeError::Blocked(format!(
                        "{error}; rollback failed: {rollback_error}"
                    ))),
                }
            }
        }
    }
}

/// Resolves an interrupted password-change transaction without either password.
/// A completely installed new set is accepted; every other unambiguous state is
/// restored from the recorded ciphertext copies.
pub fn recover_password_change(workspace: impl AsRef<Path>) -> Result<(), PasswordChangeError> {
    let _operation = stillus_platform::OperationLock::directory(workspace.as_ref())
        .map_err(|error| PasswordChangeError::Blocked(error.to_string()))?;
    let workspace = workspace.as_ref();
    let Some(directory) = active_transaction_directory(workspace)? else {
        return Ok(());
    };
    let mut transaction = Transaction::load(workspace, directory)?;
    let mut all_new = true;
    for entry in &transaction.journal.entries {
        let source = resolve_relative(workspace, &entry.source)?;
        match verified_entry_hash(workspace, entry) {
            Ok(hash) if hash == entry.new_sha256 => {}
            Ok(hash) if hash == entry.old_sha256 => all_new = false,
            Ok(_) => {
                return Err(PasswordChangeError::Blocked(format!(
                    "{} matches neither recorded version",
                    source.display()
                )));
            }
            Err(error) => {
                return Err(PasswordChangeError::Blocked(format!(
                    "{} could not be verified: {error}",
                    source.display()
                )));
            }
        }
    }
    if all_new && !transaction.journal.entries.is_empty() {
        transaction.cleanup()
    } else {
        transaction.rollback(&mut |_| {})
    }
}

impl Transaction {
    #[cfg(any(unix, windows))]
    fn create(workspace: &Path) -> Result<Self, PasswordChangeError> {
        let secure = ensure_store(workspace).map_err(save_error)?;
        let transactions = secure.join("transactions");
        ensure_private_directory(&transactions)?;
        let id = format!(
            "{:016x}{:016x}",
            std::process::id(),
            TRANSACTION_ID.fetch_add(1, Ordering::Relaxed)
        );
        let directory = transactions.join(id);
        stillus_platform::create_private_directory(&directory).map_err(io_error)?;
        sync_directory(&transactions)?;
        let journal_path = directory.join("journal.json");
        let transaction = Self {
            workspace: workspace.to_path_buf(),
            directory,
            journal_path,
            journal: Journal {
                platform: journal_platform().to_owned(),
                version: JOURNAL_VERSION,
                state: TransactionState::Preparing,
                entries: Vec::new(),
            },
        };
        transaction.save_journal()?;
        Ok(transaction)
    }

    fn load(workspace: &Path, directory: PathBuf) -> Result<Self, PasswordChangeError> {
        ensure_real_directory(&directory)?;
        let journal_path = directory.join("journal.json");
        let metadata = fs::symlink_metadata(&journal_path).map_err(io_error)?;
        if !metadata.file_type().is_file() {
            return Err(PasswordChangeError::Blocked(
                "transaction journal is not a regular file".to_owned(),
            ));
        }
        let bytes = fs::read(&journal_path).map_err(io_error)?;
        let journal: Journal = serde_json::from_slice(&bytes).map_err(|error| {
            PasswordChangeError::Blocked(format!("transaction journal is invalid: {error}"))
        })?;
        if journal.platform != journal_platform() || journal.version != JOURNAL_VERSION {
            return Err(PasswordChangeError::Blocked(format!(
                "unsupported transaction journal platform {} / version {}",
                journal.platform, journal.version
            )));
        }
        let transaction = Self {
            workspace: workspace.to_path_buf(),
            directory,
            journal_path,
            journal,
        };
        transaction.validate_journal()?;
        Ok(transaction)
    }

    #[cfg(any(unix, windows))]
    fn prepare(
        &mut self,
        snapshot: &Snapshot,
        current: &MasterPassword,
        new: &MasterPassword,
    ) -> Result<(), PasswordChangeError> {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;

        ensure_snapshot_current(snapshot)?;
        let index = self.journal.entries.len();
        let copy = self.directory.join(format!("old-{index:06}.ciphertext"));
        let copied_hash = copy_private(&snapshot.path, &copy)?;
        if copied_hash != snapshot.old_sha256 {
            let _ = fs::remove_file(&copy);
            return Err(PasswordChangeError::Conflict(format!(
                "{} changed while it was copied",
                snapshot.path.display()
            )));
        }
        let candidate = candidate_path(&snapshot.path, &self.directory, index)?;
        let prepared = (|| {
            match snapshot.kind {
                TargetKind::Note => {
                    prepare_note_candidate(&copy, &candidate, snapshot, current, new)?
                }
                TargetKind::Recovery => {
                    prepare_recovery_candidate(&copy, &candidate, snapshot, current, new)?
                }
                TargetKind::Verifier => prepare_armored_candidate(
                    &copy,
                    &candidate,
                    snapshot,
                    current,
                    new,
                    EnvelopeKind::WorkspaceVerifier,
                )?,
                TargetKind::EngineSecret => prepare_armored_candidate(
                    &copy,
                    &candidate,
                    snapshot,
                    current,
                    new,
                    EnvelopeKind::EngineSecret,
                )?,
            }
            #[cfg(windows)]
            fs::set_permissions(&candidate, snapshot.permissions.clone()).map_err(io_error)?;
            #[cfg(unix)]
            fs::set_permissions(&candidate, fs::Permissions::from_mode(snapshot.mode))
                .map_err(io_error)?;
            stillus_platform::sync_file(&candidate).map_err(io_error)?;
            if let Some(parent) = candidate.parent() {
                sync_directory(parent)?;
            }
            let new_sha256 = stable_hash(&candidate)?;
            Ok(JournalEntry {
                kind: snapshot.kind,
                source: relative_string(&self.workspace, &snapshot.path)?,
                rollback_copy: relative_string(&self.workspace, &copy)?,
                candidate: relative_string(&self.workspace, &candidate)?,
                old_sha256: snapshot.old_sha256.clone(),
                new_sha256,
                old_version: snapshot.recorded_version.clone(),
                mode: snapshot.mode,
                #[cfg(windows)]
                permissions: snapshot.permissions.clone(),
                installed: false,
            })
        })();
        let entry = match prepared {
            Ok(entry) => entry,
            Err(error) => {
                let _ = fs::remove_file(&candidate);
                let _ = fs::remove_file(&copy);
                return Err(error);
            }
        };
        self.journal.entries.push(entry);
        if let Err(error) = self.save_journal() {
            self.journal.entries.pop();
            let _ = fs::remove_file(&candidate);
            let _ = fs::remove_file(&copy);
            return Err(error);
        }
        Ok(())
    }

    fn save_journal(&self) -> Result<(), PasswordChangeError> {
        #[cfg(not(any(unix, windows)))]
        return Err(PasswordChangeError::Invalid(
            "password change is supported on Unix and Windows".to_owned(),
        ));

        #[cfg(any(unix, windows))]
        {
            #[cfg(unix)]
            use std::os::unix::fs::OpenOptionsExt;

            let temporary = self.directory.join(".journal.tmp");
            let mut options = OpenOptions::new();
            options.write(true).create_new(true).mode(0o600);
            let mut file = options.open(&temporary).map_err(io_error)?;
            let bytes = serde_json::to_vec_pretty(&self.journal).map_err(|error| {
                PasswordChangeError::Io(format!("journal serialization failed: {error}"))
            })?;
            if let Err(error) = file
                .write_all(&bytes)
                .and_then(|_| file.write_all(b"\n"))
                .and_then(|_| file.flush())
                .and_then(|_| file.sync_all())
            {
                let _ = fs::remove_file(&temporary);
                return Err(io_error(error));
            }
            drop(file);
            fs::rename(&temporary, &self.journal_path).map_err(io_error)?;
            sync_directory(&self.directory)
        }
    }

    #[cfg(any(unix, windows))]
    fn install(&mut self, index: usize) -> Result<(PathBuf, FileVersion), PasswordChangeError> {
        let entry =
            self.journal.entries.get(index).cloned().ok_or_else(|| {
                PasswordChangeError::Invalid("missing transaction entry".to_owned())
            })?;
        let source = resolve_relative(&self.workspace, &entry.source)?;
        let candidate = resolve_relative(&self.workspace, &entry.candidate)?;
        let source_metadata = fs::symlink_metadata(&source).map_err(io_error)?;
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        if !source_metadata.file_type().is_file()
            || source_metadata.nlink() != 1
            || !entry.old_version.matches(&source_metadata)
        {
            return Err(PasswordChangeError::Conflict(format!(
                "{} changed before commit",
                source.display()
            )));
        }
        if stable_hash(&source)? != entry.old_sha256 {
            return Err(PasswordChangeError::Conflict(format!(
                "{} changed before commit",
                source.display()
            )));
        }
        if stable_hash(&candidate)? != entry.new_sha256 {
            return Err(PasswordChangeError::Invalid(format!(
                "candidate for {} failed verification",
                source.display()
            )));
        }
        fs::rename(&candidate, &source).map_err(io_error)?;
        let parent = source
            .parent()
            .ok_or_else(|| PasswordChangeError::Invalid("target has no parent".to_owned()))?;
        sync_directory(parent)?;
        if verified_entry_hash(&self.workspace, &entry)? != entry.new_sha256 {
            return Err(PasswordChangeError::Blocked(format!(
                "installed candidate for {} failed verification",
                source.display()
            )));
        }
        let metadata = fs::symlink_metadata(&source).map_err(io_error)?;
        if !metadata.file_type().is_file() || metadata_nlink(&metadata) != 1 {
            return Err(PasswordChangeError::Blocked(format!(
                "installed target {} is not a regular file",
                source.display()
            )));
        }
        self.journal.entries[index].installed = true;
        self.save_journal()?;
        Ok((source, FileVersion::from_metadata(&metadata)))
    }

    fn verify_new(&self, index: usize) -> Result<(), PasswordChangeError> {
        let entry =
            self.journal.entries.get(index).ok_or_else(|| {
                PasswordChangeError::Invalid("missing transaction entry".to_owned())
            })?;
        let source = resolve_relative(&self.workspace, &entry.source)?;
        if verified_current_hash(&source)? != entry.new_sha256 {
            return Err(PasswordChangeError::Blocked(format!(
                "committed target {} failed final verification",
                source.display()
            )));
        }
        Ok(())
    }

    #[cfg(any(unix, windows))]
    fn rollback(
        &mut self,
        progress: &mut impl FnMut(PasswordChangeProgress),
    ) -> Result<(), PasswordChangeError> {
        #[cfg(unix)]
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

        self.journal.state = TransactionState::Rollback;
        self.save_journal()?;
        let total = self.journal.entries.len();
        for (completed, entry) in self.journal.entries.iter().enumerate() {
            let source = resolve_relative(&self.workspace, &entry.source)?;
            let copy = resolve_relative(&self.workspace, &entry.rollback_copy)?;
            let current_hash = verified_entry_hash(&self.workspace, entry).map_err(|error| {
                PasswordChangeError::Blocked(format!(
                    "rollback target {} cannot be read: {error}",
                    source.display()
                ))
            })?;
            if current_hash != entry.old_sha256 {
                if current_hash != entry.new_sha256 {
                    return Err(PasswordChangeError::Blocked(format!(
                        "rollback target {} is an unknown version",
                        source.display()
                    )));
                }
                let copy_metadata = fs::symlink_metadata(&copy).map_err(io_error)?;
                if !copy_metadata.file_type().is_file()
                    || copy_metadata.nlink() != 1
                    || stable_hash(&copy)? != entry.old_sha256
                {
                    return Err(PasswordChangeError::Blocked(format!(
                        "rollback copy for {} is invalid",
                        source.display()
                    )));
                }
                let parent = source.parent().ok_or_else(|| {
                    PasswordChangeError::Invalid("rollback target has no parent".to_owned())
                })?;
                let temporary = parent.join(format!(
                    ".stillus-password-rollback-{}-{completed}.tmp",
                    std::process::id()
                ));
                let mut options = OpenOptions::new();
                options.write(true).create_new(true).mode(0o600);
                let mut output = options.open(&temporary).map_err(io_error)?;
                #[cfg(windows)]
                output
                    .set_permissions(entry.permissions.clone())
                    .map_err(io_error)?;
                #[cfg(unix)]
                output
                    .set_permissions(fs::Permissions::from_mode(entry.mode))
                    .map_err(io_error)?;
                let mut input = File::open(&copy).map_err(io_error)?;
                if let Err(error) = copy_stream(&mut input, &mut output)
                    .and_then(|_| output.flush())
                    .and_then(|_| output.sync_all())
                {
                    let _ = fs::remove_file(&temporary);
                    return Err(io_error(error));
                }
                drop(output);
                fs::rename(&temporary, &source).map_err(io_error)?;
                sync_directory(parent)?;
                if stable_hash(&source)? != entry.old_sha256 {
                    return Err(PasswordChangeError::Blocked(format!(
                        "restored target {} failed verification",
                        source.display()
                    )));
                }
            }
            progress(PasswordChangeProgress {
                phase: PasswordChangePhase::RollingBack,
                completed: completed + 1,
                total,
                percent: Some(((completed + 1) as u128 * 100 / total as u128) as u8),
            });
        }
        self.cleanup()
    }

    fn cleanup(&mut self) -> Result<(), PasswordChangeError> {
        self.validate_cleanup_layout()?;
        for entry in &self.journal.entries {
            let candidate = resolve_relative(&self.workspace, &entry.candidate)?;
            match fs::symlink_metadata(&candidate) {
                Ok(metadata) if metadata.file_type().is_file() => {
                    fs::remove_file(candidate).map_err(io_error)?;
                }
                Ok(_) => {
                    return Err(PasswordChangeError::Blocked(
                        "transaction candidate is not a regular file".to_owned(),
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_error(error)),
            }
        }
        for entry in &self.journal.entries {
            let rollback_copy = resolve_relative(&self.workspace, &entry.rollback_copy)?;
            if rollback_copy.parent() != Some(self.directory.as_path()) {
                return Err(PasswordChangeError::Blocked(
                    "transaction rollback copy escaped its directory".to_owned(),
                ));
            }
            match fs::symlink_metadata(&rollback_copy) {
                Ok(metadata)
                    if metadata.file_type().is_file()
                        && stable_hash(&rollback_copy)? == entry.old_sha256 =>
                {
                    fs::remove_file(&rollback_copy).map_err(io_error)?;
                }
                Ok(_) => {
                    return Err(PasswordChangeError::Blocked(
                        "transaction rollback copy failed cleanup verification".to_owned(),
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_error(error)),
            }
        }
        fs::remove_file(&self.journal_path).map_err(io_error)?;
        let mut remaining = fs::read_dir(&self.directory).map_err(io_error)?;
        if remaining.next().is_some() {
            return Err(PasswordChangeError::Blocked(
                "transaction directory contains unknown files".to_owned(),
            ));
        }
        let parent = self.directory.parent().map(Path::to_path_buf);
        fs::remove_dir(&self.directory).map_err(io_error)?;
        if let Some(parent) = parent {
            sync_directory(&parent)?;
        }
        Ok(())
    }

    fn validate_journal(&self) -> Result<(), PasswordChangeError> {
        #[cfg(any(unix, windows))]
        {
            #[cfg(unix)]
            use std::os::unix::fs::{MetadataExt, PermissionsExt};

            let directory = fs::symlink_metadata(&self.directory).map_err(io_error)?;
            if directory.permissions().mode() & 0o077 != 0 {
                return Err(PasswordChangeError::Blocked(
                    "transaction directory is not private".to_owned(),
                ));
            }
            let journal = fs::symlink_metadata(&self.journal_path).map_err(io_error)?;
            if journal.nlink() != 1 || journal.permissions().mode() & 0o077 != 0 {
                return Err(PasswordChangeError::Blocked(
                    "transaction journal is not private and unlinked".to_owned(),
                ));
            }
        }

        let transaction_id = self
            .directory
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| PasswordChangeError::Blocked("invalid transaction id".to_owned()))?;
        if transaction_id.len() != 32
            || !transaction_id.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(PasswordChangeError::Blocked(
                "invalid transaction directory name".to_owned(),
            ));
        }
        let mut paths = HashSet::new();
        for (index, entry) in self.journal.entries.iter().enumerate() {
            if entry.old_sha256.len() != 64
                || entry.new_sha256.len() != 64
                || !entry
                    .old_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
                || !entry
                    .new_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
                || entry.mode > 0o7777
            {
                return Err(PasswordChangeError::Blocked(
                    "transaction journal contains invalid target metadata".to_owned(),
                ));
            }
            let source = resolve_relative(&self.workspace, &entry.source)?;
            let expected_parent = match entry.kind {
                TargetKind::Note => self.workspace.join("notes"),
                TargetKind::Recovery => self.workspace.join(".stillus/recovery"),
                TargetKind::Verifier => self.workspace.join(".stillus_security"),
                TargetKind::EngineSecret => self.workspace.join(".stillus_security/secrets"),
            };
            let expected_extension = match entry.kind {
                TargetKind::Note => "md",
                TargetKind::Recovery => "nrrec",
                TargetKind::Verifier | TargetKind::EngineSecret => "age",
            };
            if source.parent() != Some(expected_parent.as_path())
                || source.extension().and_then(|value| value.to_str()) != Some(expected_extension)
                || (entry.kind == TargetKind::Verifier
                    && source.file_name().and_then(|value| value.to_str()) != Some("master.age"))
                || (entry.kind == TargetKind::EngineSecret && !valid_secret_filename(&source))
            {
                return Err(PasswordChangeError::Blocked(
                    "transaction target has an invalid location".to_owned(),
                ));
            }
            let rollback_copy = resolve_relative(&self.workspace, &entry.rollback_copy)?;
            let expected_copy_name = format!("old-{index:06}.ciphertext");
            if rollback_copy.parent() != Some(self.directory.as_path())
                || rollback_copy.file_name().and_then(|name| name.to_str())
                    != Some(expected_copy_name.as_str())
            {
                return Err(PasswordChangeError::Blocked(
                    "transaction rollback path is invalid".to_owned(),
                ));
            }
            let candidate = resolve_relative(&self.workspace, &entry.candidate)?;
            let expected_candidate = candidate_path(&source, &self.directory, index)?;
            if candidate != expected_candidate
                || !paths.insert(source)
                || !paths.insert(rollback_copy)
                || !paths.insert(candidate)
            {
                return Err(PasswordChangeError::Blocked(
                    "transaction journal contains duplicate or invalid paths".to_owned(),
                ));
            }
        }
        Ok(())
    }

    fn validate_cleanup_layout(&self) -> Result<(), PasswordChangeError> {
        self.validate_journal()?;
        let mut allowed = HashSet::from([PathBuf::from("journal.json")]);
        let mut all_old = true;
        let mut all_new = true;
        for entry in &self.journal.entries {
            let hash = verified_entry_hash(&self.workspace, entry)?;
            all_old &= hash == entry.old_sha256;
            all_new &= hash == entry.new_sha256;
            let rollback_copy = resolve_relative(&self.workspace, &entry.rollback_copy)?;
            allowed.insert(
                rollback_copy
                    .file_name()
                    .map(PathBuf::from)
                    .ok_or_else(|| {
                        PasswordChangeError::Blocked("invalid rollback copy".to_owned())
                    })?,
            );
            let candidate = resolve_relative(&self.workspace, &entry.candidate)?;
            if let Ok(metadata) = fs::symlink_metadata(&candidate)
                && (!metadata.file_type().is_file()
                    || metadata_nlink(&metadata) != 1
                    || stable_hash(&candidate)? != entry.new_sha256)
            {
                return Err(PasswordChangeError::Blocked(
                    "transaction candidate failed cleanup verification".to_owned(),
                ));
            }
        }
        if !all_old && !all_new && !self.journal.entries.is_empty() {
            return Err(PasswordChangeError::Blocked(
                "transaction targets are mixed during cleanup".to_owned(),
            ));
        }
        for child in fs::read_dir(&self.directory).map_err(io_error)? {
            let child = child.map_err(io_error)?;
            if !allowed.contains(&PathBuf::from(child.file_name())) {
                return Err(PasswordChangeError::Blocked(format!(
                    "transaction directory contains unknown file {}",
                    child.path().display()
                )));
            }
        }
        Ok(())
    }
}

#[cfg(any(unix, windows))]
fn preflight_note(
    workspace: &Path,
    target: &PasswordChangeTarget,
    password: &MasterPassword,
) -> Result<Snapshot, PasswordChangeError> {
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    validate_workspace_file(workspace, &target.path)?;
    let (mut file, version) = open_versioned(&target.path).map_err(save_error)?;
    let metadata = file.metadata().map_err(io_error)?;
    if metadata.nlink() != 1 {
        return Err(PasswordChangeError::Invalid(format!(
            "{} is hard-linked",
            target.path.display()
        )));
    }
    if version != target.version {
        return Err(PasswordChangeError::Conflict(format!(
            "{} changed before validation",
            target.path.display()
        )));
    }
    let scan =
        scan_reader(&mut file).map_err(|error| PasswordChangeError::Invalid(error.to_string()))?;
    let body_offset = match &scan.status {
        FrontMatterStatus::Parsed(parsed)
            if parsed.metadata.encryption == Some(EncryptionFormat::AgeBodyV1) =>
        {
            parsed.body_offset
        }
        _ => {
            return Err(PasswordChangeError::Invalid(format!(
                "{} is not an available protected note",
                target.path.display()
            )));
        }
    };
    file.seek(SeekFrom::Start(body_offset)).map_err(io_error)?;
    let (plaintext_sha256, plaintext_len) =
        hash_reader(decrypt_body(file, password).map_err(|_| {
            PasswordChangeError::Invalid("current password is incorrect".to_owned())
        })?)
        .map_err(|_| PasswordChangeError::Invalid("current password is incorrect".to_owned()))?;
    ensure_version(&target.path, &target.version)?;
    Ok(Snapshot {
        kind: TargetKind::Note,
        path: target.path.clone(),
        version: target.version,
        old_sha256: stable_hash(&target.path)?,
        plaintext_sha256,
        plaintext_len,
        mode: metadata.permissions().mode() & 0o7777,
        #[cfg(windows)]
        permissions: metadata.permissions(),
        recorded_version: RecordedFileVersion::from_metadata(&metadata),
    })
}

#[cfg(any(unix, windows))]
fn preflight_recovery(
    workspace: &Path,
    path: &Path,
    password: &MasterPassword,
) -> Result<Snapshot, PasswordChangeError> {
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    validate_workspace_file(workspace, path)?;
    let (file, version) = open_versioned(path).map_err(save_error)?;
    let metadata = file.metadata().map_err(io_error)?;
    if metadata.nlink() != 1 {
        return Err(PasswordChangeError::Invalid(format!(
            "{} is hard-linked",
            path.display()
        )));
    }
    let reader = decrypt(file, password, EnvelopeKind::Recovery)
        .map_err(|_| PasswordChangeError::Invalid("current password is incorrect".to_owned()))?;
    let (plaintext_sha256, plaintext_len) = hash_reader(reader)
        .map_err(|_| PasswordChangeError::Invalid("current password is incorrect".to_owned()))?;
    ensure_version(path, &version)?;
    Ok(Snapshot {
        kind: TargetKind::Recovery,
        path: path.to_path_buf(),
        version,
        old_sha256: stable_hash(path)?,
        plaintext_sha256,
        plaintext_len,
        mode: metadata.permissions().mode() & 0o7777,
        #[cfg(windows)]
        permissions: metadata.permissions(),
        recorded_version: RecordedFileVersion::from_metadata(&metadata),
    })
}

#[cfg(any(unix, windows))]
fn preflight_armored(
    workspace: &Path,
    target: &PasswordChangeTarget,
    password: &MasterPassword,
    kind: TargetKind,
    envelope_kind: EnvelopeKind,
) -> Result<Snapshot, PasswordChangeError> {
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    validate_workspace_file(workspace, &target.path)?;
    let (file, version) = open_versioned(&target.path).map_err(save_error)?;
    let metadata = file.metadata().map_err(io_error)?;
    if metadata.nlink() != 1 {
        return Err(PasswordChangeError::Invalid(format!(
            "{} is hard-linked",
            target.path.display()
        )));
    }
    if version != target.version {
        return Err(PasswordChangeError::Conflict(format!(
            "{} changed before validation",
            target.path.display()
        )));
    }
    let reader = decrypt_armored(file, password, envelope_kind)
        .map_err(|_| PasswordChangeError::Invalid("current password is incorrect".to_owned()))?;
    let (plaintext_sha256, plaintext_len) = hash_reader(reader)
        .map_err(|_| PasswordChangeError::Invalid("current password is incorrect".to_owned()))?;
    ensure_version(&target.path, &target.version)?;
    Ok(Snapshot {
        kind,
        path: target.path.clone(),
        version: target.version,
        old_sha256: stable_hash(&target.path)?,
        plaintext_sha256,
        plaintext_len,
        mode: metadata.permissions().mode() & 0o7777,
        #[cfg(windows)]
        permissions: metadata.permissions(),
        recorded_version: RecordedFileVersion::from_metadata(&metadata),
    })
}

#[cfg(any(unix, windows))]
fn prepare_note_candidate(
    copy: &Path,
    candidate: &Path,
    snapshot: &Snapshot,
    current: &MasterPassword,
    new: &MasterPassword,
) -> Result<(), PasswordChangeError> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    let mut input = File::open(copy).map_err(io_error)?;
    let scan =
        scan_reader(&mut input).map_err(|error| PasswordChangeError::Invalid(error.to_string()))?;
    let body_offset = match &scan.status {
        FrontMatterStatus::Parsed(parsed)
            if parsed.metadata.encryption == Some(EncryptionFormat::AgeBodyV1) =>
        {
            parsed.body_offset
        }
        _ => {
            return Err(PasswordChangeError::Invalid(
                "protected copy is invalid".to_owned(),
            ));
        }
    };
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut output = options.open(candidate).map_err(io_error)?;
    input.seek(SeekFrom::Start(0)).map_err(io_error)?;
    copy_stream(
        &mut std::io::Read::by_ref(&mut input).take(body_offset),
        &mut output,
    )
    .map_err(io_error)?;
    input.seek(SeekFrom::Start(body_offset)).map_err(io_error)?;
    let mut decrypted = decrypt_body(input, current)
        .map_err(|_| PasswordChangeError::Invalid("current password is incorrect".to_owned()))?;
    let mut encrypted = body_writer(output, new, snapshot.plaintext_len)
        .map_err(|_| PasswordChangeError::Invalid("new password is invalid".to_owned()))?;
    copy_stream(&mut decrypted, &mut encrypted).map_err(io_error)?;
    output = encrypted
        .finish()
        .map_err(|_| PasswordChangeError::Invalid("candidate encryption failed".to_owned()))?;
    output.flush().map_err(io_error)?;
    output.sync_all().map_err(io_error)?;
    drop(output);
    verify_note_candidate(candidate, body_offset, snapshot, new)
}

fn verify_note_candidate(
    candidate: &Path,
    body_offset: u64,
    snapshot: &Snapshot,
    new: &MasterPassword,
) -> Result<(), PasswordChangeError> {
    let mut file = File::open(candidate).map_err(io_error)?;
    file.seek(SeekFrom::Start(body_offset)).map_err(io_error)?;
    let reader = decrypt_body(file, new)
        .map_err(|_| PasswordChangeError::Invalid("candidate verification failed".to_owned()))?;
    let (hash, len) = hash_reader(reader).map_err(io_error)?;
    if hash != snapshot.plaintext_sha256 || len != snapshot.plaintext_len {
        return Err(PasswordChangeError::Invalid(
            "note candidate plaintext mismatch".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(any(unix, windows))]
fn prepare_recovery_candidate(
    copy: &Path,
    candidate: &Path,
    snapshot: &Snapshot,
    current: &MasterPassword,
    new: &MasterPassword,
) -> Result<(), PasswordChangeError> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    let input = File::open(copy).map_err(io_error)?;
    let mut decrypted = decrypt(input, current, EnvelopeKind::Recovery)
        .map_err(|_| PasswordChangeError::Invalid("current password is incorrect".to_owned()))?;
    let metadata = decrypted.metadata().clone();
    if metadata.payload_len != snapshot.plaintext_len {
        return Err(PasswordChangeError::Invalid(
            "recovery payload length changed".to_owned(),
        ));
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let output = options.open(candidate).map_err(io_error)?;
    let mut encrypted = envelope_writer(output, new, metadata)
        .map_err(|_| PasswordChangeError::Invalid("new password is invalid".to_owned()))?;
    copy_stream(&mut decrypted, &mut encrypted).map_err(io_error)?;
    let mut output = encrypted
        .finish()
        .map_err(|_| PasswordChangeError::Invalid("candidate encryption failed".to_owned()))?;
    output.flush().map_err(io_error)?;
    output.sync_all().map_err(io_error)?;
    drop(output);

    let reader = decrypt(
        File::open(candidate).map_err(io_error)?,
        new,
        EnvelopeKind::Recovery,
    )
    .map_err(|_| PasswordChangeError::Invalid("candidate verification failed".to_owned()))?;
    let (hash, len) = hash_reader(reader).map_err(io_error)?;
    if hash != snapshot.plaintext_sha256 || len != snapshot.plaintext_len {
        return Err(PasswordChangeError::Invalid(
            "recovery candidate plaintext mismatch".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(any(unix, windows))]
fn prepare_armored_candidate(
    copy: &Path,
    candidate: &Path,
    snapshot: &Snapshot,
    current: &MasterPassword,
    new: &MasterPassword,
    kind: EnvelopeKind,
) -> Result<(), PasswordChangeError> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    let input = File::open(copy).map_err(io_error)?;
    let mut decrypted = decrypt_armored(input, current, kind)
        .map_err(|_| PasswordChangeError::Invalid("current password is incorrect".to_owned()))?;
    let metadata = decrypted.metadata().clone();
    if metadata.payload_len != snapshot.plaintext_len {
        return Err(PasswordChangeError::Invalid(
            "workspace security payload length changed".to_owned(),
        ));
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let output = options.open(candidate).map_err(io_error)?;
    let mut encrypted = armored_envelope_writer(output, new, metadata)
        .map_err(|_| PasswordChangeError::Invalid("new password is invalid".to_owned()))?;
    copy_stream(&mut decrypted, &mut encrypted).map_err(io_error)?;
    let mut output = encrypted
        .finish()
        .map_err(|_| PasswordChangeError::Invalid("candidate encryption failed".to_owned()))?;
    output.flush().map_err(io_error)?;
    output.sync_all().map_err(io_error)?;
    drop(output);

    let reader = decrypt_armored(File::open(candidate).map_err(io_error)?, new, kind)
        .map_err(|_| PasswordChangeError::Invalid("candidate verification failed".to_owned()))?;
    let (hash, len) = hash_reader(reader).map_err(io_error)?;
    if hash != snapshot.plaintext_sha256 || len != snapshot.plaintext_len {
        return Err(PasswordChangeError::Invalid(
            "workspace security candidate plaintext mismatch".to_owned(),
        ));
    }
    Ok(())
}

fn body_writer<W: Write>(
    output: W,
    password: &MasterPassword,
    body_len: u64,
) -> Result<BodyEnvelopeWriter<W>, stillus_secure::SecureError> {
    #[cfg(any(test, feature = "test-utils"))]
    {
        BodyEnvelopeWriter::new_for_test(output, password, body_len)
    }
    #[cfg(not(any(test, feature = "test-utils")))]
    {
        BodyEnvelopeWriter::new(output, password, body_len)
    }
}

fn envelope_writer<W: Write>(
    output: W,
    password: &MasterPassword,
    metadata: EnvelopeMetadata,
) -> Result<EnvelopeWriter<W>, stillus_secure::SecureError> {
    #[cfg(any(test, feature = "test-utils"))]
    {
        EnvelopeWriter::new_for_test(output, password, metadata)
    }
    #[cfg(not(any(test, feature = "test-utils")))]
    {
        EnvelopeWriter::new(output, password, metadata)
    }
}

fn armored_envelope_writer<W: Write>(
    output: W,
    password: &MasterPassword,
    metadata: EnvelopeMetadata,
) -> Result<ArmoredEnvelopeWriter<W>, stillus_secure::SecureError> {
    #[cfg(any(test, feature = "test-utils"))]
    {
        ArmoredEnvelopeWriter::new_for_test(output, password, metadata)
    }
    #[cfg(not(any(test, feature = "test-utils")))]
    {
        ArmoredEnvelopeWriter::new(output, password, metadata)
    }
}

fn valid_secret_filename(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .and_then(|value| value.strip_suffix(".age"))
        .is_some_and(|value| {
            value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

fn ensure_snapshot_current(snapshot: &Snapshot) -> Result<(), PasswordChangeError> {
    ensure_version(&snapshot.path, &snapshot.version)?;
    if stable_hash(&snapshot.path)? != snapshot.old_sha256 {
        return Err(PasswordChangeError::Conflict(format!(
            "{} changed after validation",
            snapshot.path.display()
        )));
    }
    Ok(())
}

fn ensure_version(path: &Path, expected: &FileVersion) -> Result<(), PasswordChangeError> {
    let (_, version) = open_versioned(path).map_err(save_error)?;
    if &version != expected {
        return Err(PasswordChangeError::Conflict(format!(
            "{} changed during password change",
            path.display()
        )));
    }
    Ok(())
}

fn validate_workspace_file(workspace: &Path, path: &Path) -> Result<(), PasswordChangeError> {
    let relative = path.strip_prefix(workspace).map_err(|_| {
        PasswordChangeError::Invalid(format!("{} is outside the workspace", path.display()))
    })?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(PasswordChangeError::Invalid(format!(
            "{} is not a safe workspace path",
            path.display()
        )));
    }
    let metadata = fs::symlink_metadata(path).map_err(io_error)?;
    if !metadata.file_type().is_file() {
        return Err(PasswordChangeError::Invalid(format!(
            "{} must be a regular file",
            path.display()
        )));
    }
    Ok(())
}

fn reject_active_transaction(workspace: &Path) -> Result<(), PasswordChangeError> {
    if active_transaction_directory(workspace)?.is_some() {
        return Err(PasswordChangeError::Blocked(
            "another password change transaction is active".to_owned(),
        ));
    }
    Ok(())
}

fn active_transaction_directory(workspace: &Path) -> Result<Option<PathBuf>, PasswordChangeError> {
    let transactions = workspace.join(".stillus_backups/secure/transactions");
    let metadata = match fs::symlink_metadata(&transactions) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error)),
    };
    if !metadata.file_type().is_dir() {
        return Err(PasswordChangeError::Blocked(
            "transactions path is not a real directory".to_owned(),
        ));
    }
    let mut active = Vec::new();
    for entry in fs::read_dir(&transactions).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(io_error)?;
        if !metadata.file_type().is_dir() {
            return Err(PasswordChangeError::Blocked(format!(
                "unknown entry in transactions directory: {}",
                entry.path().display()
            )));
        }
        active.push(entry.path());
    }
    match active.len() {
        0 => Ok(None),
        1 => Ok(active.pop()),
        _ => Err(PasswordChangeError::Blocked(
            "multiple password change transactions are present".to_owned(),
        )),
    }
}

fn candidate_path(
    source: &Path,
    transaction_directory: &Path,
    index: usize,
) -> Result<PathBuf, PasswordChangeError> {
    let id = transaction_directory
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| PasswordChangeError::Invalid("transaction id is invalid".to_owned()))?;
    let parent = source
        .parent()
        .ok_or_else(|| PasswordChangeError::Invalid("target has no parent".to_owned()))?;
    Ok(parent.join(format!(".stillus-password-{id}-{index:06}.tmp")))
}

fn relative_string(workspace: &Path, path: &Path) -> Result<String, PasswordChangeError> {
    let relative = path.strip_prefix(workspace).map_err(|_| {
        PasswordChangeError::Invalid("transaction path escaped workspace".to_owned())
    })?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(PasswordChangeError::Invalid(
            "transaction path is invalid".to_owned(),
        ));
    }
    relative
        .to_str()
        .map(|value| value.replace(std::path::MAIN_SEPARATOR, "/"))
        .ok_or_else(|| PasswordChangeError::Invalid("transaction path is not UTF-8".to_owned()))
}

fn resolve_relative(workspace: &Path, value: &str) -> Result<PathBuf, PasswordChangeError> {
    let relative = PathBuf::from(value);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(PasswordChangeError::Blocked(
            "transaction journal contains an invalid path".to_owned(),
        ));
    }
    Ok(workspace.join(relative))
}

#[cfg(any(unix, windows))]
fn copy_private(source: &Path, destination: &Path) -> Result<String, PasswordChangeError> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    let mut input = File::open(source).map_err(io_error)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut output = options.open(destination).map_err(io_error)?;
    let hash = copy_hash(&mut input, &mut output)?;
    output.flush().map_err(io_error)?;
    output.sync_all().map_err(io_error)?;
    Ok(hash)
}

fn stable_hash(path: &Path) -> Result<String, PasswordChangeError> {
    let before = fs::symlink_metadata(path).map_err(io_error)?;
    if !before.file_type().is_file() {
        return Err(PasswordChangeError::Invalid(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let mut file = File::open(path).map_err(io_error)?;
    let (hash, _) = hash_reader(&mut file).map_err(io_error)?;
    let after = fs::symlink_metadata(path).map_err(io_error)?;
    if FileVersion::from_metadata(&before) != FileVersion::from_metadata(&after) {
        return Err(PasswordChangeError::Conflict(format!(
            "{} changed while hashing",
            path.display()
        )));
    }
    Ok(hash)
}

fn verified_current_hash(path: &Path) -> Result<String, PasswordChangeError> {
    let metadata = fs::symlink_metadata(path).map_err(io_error)?;
    if !metadata.file_type().is_file() || metadata_nlink(&metadata) != 1 {
        return Err(PasswordChangeError::Blocked(format!(
            "{} is not an unlinked regular file",
            path.display()
        )));
    }
    stable_hash(path)
}

fn verified_entry_hash(
    workspace: &Path,
    entry: &JournalEntry,
) -> Result<String, PasswordChangeError> {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let path = resolve_relative(workspace, &entry.source)?;
    let metadata = fs::symlink_metadata(&path).map_err(io_error)?;
    #[cfg(any(unix, windows))]
    #[cfg(windows)]
    if metadata.permissions() != entry.permissions {
        return Err(PasswordChangeError::Blocked(
            "Windows ACL no longer matches the transaction journal".to_owned(),
        ));
    }
    if metadata.permissions().mode() & 0o7777 != entry.mode {
        return Err(PasswordChangeError::Blocked(format!(
            "{} permissions do not match the transaction journal",
            path.display()
        )));
    }
    verified_current_hash(&path)
}

#[cfg(any(unix, windows))]
fn metadata_nlink(metadata: &fs::Metadata) -> u64 {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    metadata.nlink()
}

#[cfg(not(any(unix, windows)))]
fn metadata_nlink(_metadata: &fs::Metadata) -> u64 {
    1
}

fn copy_hash(
    input: &mut impl Read,
    output: &mut impl Write,
) -> Result<String, PasswordChangeError> {
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; BUFFER_BYTES];
    loop {
        let read = input.read(&mut buffer).map_err(io_error)?;
        if read == 0 {
            break;
        }
        output.write_all(&buffer[..read]).map_err(io_error)?;
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn hash_reader(mut input: impl Read) -> io::Result<(String, u64)> {
    let mut hasher = Sha256::new();
    let mut length = 0_u64;
    let mut buffer = [0_u8; BUFFER_BYTES];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        length = length
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("input is too large"))?;
        hasher.update(&buffer[..read]);
    }
    Ok((format!("{:x}", hasher.finalize()), length))
}

fn copy_stream(input: &mut impl Read, output: &mut impl Write) -> io::Result<()> {
    let mut buffer = [0_u8; BUFFER_BYTES];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        output.write_all(&buffer[..read])?;
    }
}

fn ensure_private_directory(path: &Path) -> Result<(), PasswordChangeError> {
    #[cfg(any(unix, windows))]
    {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        match fs::symlink_metadata(path) {
            Ok(metadata)
                if metadata.file_type().is_dir() && metadata.permissions().mode() & 0o077 == 0 =>
            {
                return Ok(());
            }
            Ok(_) => {
                return Err(PasswordChangeError::Blocked(format!(
                    "{} is not a real directory",
                    path.display()
                )));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(error)),
        }
        stillus_platform::create_private_directory(path).map_err(io_error)?;
        if let Some(parent) = path.parent() {
            sync_directory(parent)?;
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(PasswordChangeError::Invalid(
            "password change is supported on Unix and Windows".to_owned(),
        ))
    }
}

fn ensure_real_directory(path: &Path) -> Result<(), PasswordChangeError> {
    let metadata = fs::symlink_metadata(path).map_err(io_error)?;
    if !metadata.file_type().is_dir() {
        return Err(PasswordChangeError::Blocked(format!(
            "{} is not a real directory",
            path.display()
        )));
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), PasswordChangeError> {
    stillus_platform::sync_directory(path).map_err(io_error)
}

fn save_error(error: SaveError) -> PasswordChangeError {
    match error {
        SaveError::Conflict => PasswordChangeError::Conflict(error.to_string()),
        _ => PasswordChangeError::Io(error.to_string()),
    }
}

fn io_error(error: impl std::fmt::Display) -> PasswordChangeError {
    PasswordChangeError::Io(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protect_note_body;
    use stillus_engine::{EngineId, ItemId, SecretValue};
    use stillus_security::{SecretBinding, SecurityError, SecurityStore};

    fn workspace(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "stillus-password-change-{name}-{}-{}",
            std::process::id(),
            TRANSACTION_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("notes")).unwrap();
        root
    }

    fn protected_note(
        root: &Path,
        name: &str,
        password: &MasterPassword,
    ) -> (PathBuf, FileVersion) {
        let path = root.join("notes").join(name);
        fs::write(
            &path,
            b"---\ntitle: Private\nfuture: keep\ndeleted: true\n---\nsecret body\n",
        )
        .unwrap();
        let version = open_versioned(&path).unwrap().1;
        protect_note_body(&path, &version, password, "Private").unwrap();
        let version = open_versioned(&path).unwrap().1;
        (path, version)
    }

    #[test]
    fn another_window_cannot_recover_a_live_password_transaction() {
        use std::sync::mpsc;
        use std::time::Duration;
        let root = workspace("concurrent-recovery");
        let old = MasterPassword::new("old concurrent password".to_owned());
        let new = MasterPassword::new("new concurrent password".to_owned());
        let (path, version) = protected_note(&root, "Private.md", &old);
        let target = PasswordChangeTarget {
            path: path.clone(),
            version,
        };
        let (started, receiving) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        let changing_root = root.clone();
        let worker = std::thread::spawn(move || {
            let mut paused = false;
            change_master_password(&changing_root, &[target], &[], &old, &new, |_| {
                if !paused {
                    paused = true;
                    started.send(()).unwrap();
                    resumed.recv().unwrap();
                }
            })
            .unwrap();
        });
        receiving.recv_timeout(Duration::from_secs(5)).unwrap();
        let (finished, finishing) = mpsc::channel();
        let recovery_root = root.clone();
        let recovery = std::thread::spawn(move || {
            recover_password_change(recovery_root).unwrap();
            finished.send(()).unwrap();
        });
        assert!(finishing.recv_timeout(Duration::from_millis(50)).is_err());
        resume.send(()).unwrap();
        worker.join().unwrap();
        finishing.recv_timeout(Duration::from_secs(5)).unwrap();
        recovery.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rotates_verifier_and_engine_secret_without_notes() {
        let root = workspace("verifier-secret");
        let old = MasterPassword::new("old password".to_owned());
        let new = MasterPassword::new("new password".to_owned());
        let store = SecurityStore::new(&root);
        let vault_id = store.configure(&old).unwrap();
        let binding = SecretBinding::new(
            EngineId::new("test").unwrap(),
            ItemId::new("items/one").unwrap(),
            "connection/token",
        )
        .unwrap();
        let value = SecretValue::new(b"rotated secret".to_vec()).unwrap();
        let reference = store.store_secret(&binding, &value, &old).unwrap();
        let verifier_path = store.verifier_path();
        let secret_path = store.secret_path(&reference);
        let old_secret_ciphertext = fs::read(&secret_path).unwrap();
        let verifier = PasswordChangeTarget {
            version: open_versioned(&verifier_path).unwrap().1,
            path: verifier_path,
        };
        let secret = PasswordChangeTarget {
            version: open_versioned(&secret_path).unwrap().1,
            path: secret_path,
        };
        let mut progress = Vec::new();
        let commit = rotate_workspace_security(
            &root,
            SecurityRotationTargets {
                verifier: Some(&verifier),
                secrets: &[secret],
                notes: &[],
                recovery: &[],
            },
            &old,
            &new,
            |event| progress.push(event),
        )
        .unwrap();
        assert_eq!(commit.secret_count, 1);
        assert!(matches!(
            store.unlock(&old),
            Err(SecurityError::AuthenticationFailed)
        ));
        assert_eq!(store.unlock(&new).unwrap(), vault_id);
        assert_eq!(
            store
                .resolve_secret(&reference, &binding, &new)
                .unwrap()
                .expose(),
            b"rotated secret"
        );
        assert!(progress.iter().any(|event| {
            event.phase == PasswordChangePhase::PreparingVerifier
                && event.completed == 1
                && event.total == 1
        }));
        assert!(progress.iter().any(|event| {
            event.phase == PasswordChangePhase::BackingUpSecrets
                && event.completed == 1
                && event.total == 1
        }));
        assert!(progress.iter().any(|event| {
            event.phase == PasswordChangePhase::Verifying
                && event.completed == 2
                && event.total == 2
                && event.percent == Some(99)
        }));
        assert!(
            progress
                .iter()
                .take_while(|event| event.phase == PasswordChangePhase::Validating)
                .all(|event| event.percent == Some(0))
        );
        assert!(progress.iter().any(|event| {
            event.phase == PasswordChangePhase::PreparingSecrets
                && event.completed == event.total
                && event.percent.is_some_and(|percent| percent >= 80)
        }));
        assert!(
            progress
                .iter()
                .filter_map(|event| event.percent)
                .all(|percent| percent <= 99)
        );
        assert!(
            progress
                .iter()
                .filter_map(|event| event.percent)
                .collect::<Vec<_>>()
                .windows(2)
                .all(|values| values[0] <= values[1])
        );
        let secure_backups = root.join(".stillus_backups/secure");
        assert!(fs::read_dir(secure_backups).unwrap().any(|entry| {
            let Ok(entry) = entry else {
                return false;
            };
            entry.path().is_dir()
                && fs::read_dir(entry.path()).unwrap().any(|candidate| {
                    candidate
                        .ok()
                        .filter(|candidate| candidate.path().extension() == Some("age".as_ref()))
                        .and_then(|candidate| fs::read(candidate.path()).ok())
                        .is_some_and(|bytes| bytes == old_secret_ciphertext)
                })
        }));
        fs::remove_dir_all(root).unwrap();
    }

    fn decrypted_body(path: &Path, password: &MasterPassword) -> Result<Vec<u8>, ()> {
        let mut file = File::open(path).unwrap();
        let scan = scan_reader(&mut file).unwrap();
        let offset = match scan.status {
            FrontMatterStatus::Parsed(parsed) => parsed.body_offset,
            _ => return Err(()),
        };
        file.seek(SeekFrom::Start(offset)).unwrap();
        let mut reader = decrypt_body(file, password).map_err(|_| ())?;
        let mut body = Vec::new();
        reader.read_to_end(&mut body).map_err(|_| ())?;
        Ok(body)
    }

    fn protected_recovery(root: &Path, password: &MasterPassword) -> (PathBuf, Vec<u8>) {
        let directory = root.join(".stillus/recovery");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("active.nrrec");
        let plaintext = b"record header\nrecovery body\n".to_vec();
        let metadata = EnvelopeMetadata::new(
            EnvelopeKind::Recovery,
            "active.nrrec".to_owned(),
            plaintext.len() as u64,
        )
        .unwrap();
        let output = File::create(&path).unwrap();
        let mut encrypted = envelope_writer(output, password, metadata).unwrap();
        encrypted.write_all(&plaintext).unwrap();
        encrypted.finish().unwrap().sync_all().unwrap();
        (path, plaintext)
    }

    #[test]
    fn changes_note_password_preserves_prefix_and_creates_old_ciphertext_backup() {
        let root = workspace("commit");
        let old = MasterPassword::new("old password".to_owned());
        let new = MasterPassword::new("new password".to_owned());
        let (path, version) = protected_note(&root, "Private.md", &old);
        let old_bytes = fs::read(&path).unwrap();
        let mut old_file = File::open(&path).unwrap();
        let old_prefix_len = match scan_reader(&mut old_file).unwrap().status {
            FrontMatterStatus::Parsed(parsed) => parsed.body_offset as usize,
            _ => panic!("protected note has parsed front matter"),
        };
        drop(old_file);
        let mut progress = Vec::new();
        let commit = change_master_password(
            &root,
            &[PasswordChangeTarget {
                path: path.clone(),
                version,
            }],
            &[],
            &old,
            &new,
            |event| progress.push(event),
        )
        .unwrap();
        assert_eq!(commit.note_count, 1);
        assert_eq!(commit.recovery_count, 0);
        assert_eq!(decrypted_body(&path, &new).unwrap(), b"secret body\n");
        assert!(decrypted_body(&path, &old).is_err());
        assert_eq!(
            &fs::read(&path).unwrap()[..old_prefix_len],
            &old_bytes[..old_prefix_len]
        );
        assert!(progress.windows(2).all(|events| {
            events[0].phase != events[1].phase || events[0].completed <= events[1].completed
        }));
        assert!(
            progress
                .iter()
                .filter(|event| event.phase == PasswordChangePhase::Validating)
                .all(|event| event.percent == Some(0))
        );
        assert!(progress.iter().any(|event| {
            event.phase == PasswordChangePhase::PreparingNotes
                && event.completed == 1
                && event.total == 1
                && event.percent == Some(80)
        }));
        let estimated = progress
            .iter()
            .filter_map(|event| event.percent)
            .collect::<Vec<_>>();
        assert_eq!(estimated.first(), Some(&0));
        assert_eq!(estimated.last(), Some(&99));
        assert!(estimated.windows(2).all(|values| values[0] <= values[1]));
        let backup_found = fs::read_dir(root.join(".stillus_backups/secure"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .flat_map(|entry| fs::read_dir(entry.path()).into_iter().flatten())
            .filter_map(Result::ok)
            .any(|entry| fs::read(entry.path()).ok().as_deref() == Some(old_bytes.as_slice()));
        assert!(backup_found);
        assert!(active_transaction_directory(&root).unwrap().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wrong_password_leaves_workspace_byte_identical() {
        let root = workspace("wrong");
        let old = MasterPassword::new("old password".to_owned());
        let wrong = MasterPassword::new("wrong password".to_owned());
        let new = MasterPassword::new("new password".to_owned());
        let (path, version) = protected_note(&root, "Private.md", &old);
        let before = fs::read(&path).unwrap();
        let error = change_master_password(
            &root,
            &[PasswordChangeTarget {
                path: path.clone(),
                version,
            }],
            &[],
            &wrong,
            &new,
            |_| {},
        )
        .unwrap_err();
        assert!(matches!(error, PasswordChangeError::Invalid(_)));
        assert_eq!(fs::read(path).unwrap(), before);
        assert!(!root.join(".stillus_backups").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn empty_or_reused_new_password_is_rejected_before_writes() {
        let root = workspace("invalid-new");
        let old = MasterPassword::new("old password".to_owned());
        let empty = MasterPassword::new(String::new());
        let (path, version) = protected_note(&root, "Private.md", &old);
        let before = fs::read(&path).unwrap();
        for new in [empty, old.clone()] {
            assert!(matches!(
                change_master_password(
                    &root,
                    &[PasswordChangeTarget {
                        path: path.clone(),
                        version,
                    }],
                    &[],
                    &old,
                    &new,
                    |_| {},
                ),
                Err(PasswordChangeError::Invalid(_))
            ));
            assert_eq!(fs::read(&path).unwrap(), before);
            assert!(!root.join(".stillus_backups").exists());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn changes_recovery_password_without_changing_authenticated_plaintext() {
        use std::os::unix::fs::PermissionsExt;

        let root = workspace("recovery");
        let old = MasterPassword::new("old recovery password".to_owned());
        let new = MasterPassword::new("new recovery password".to_owned());
        let (path, plaintext) = protected_recovery(&root, &old);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let before = fs::read(&path).unwrap();
        let mut progress = Vec::new();
        let commit = change_master_password(&root, &[], &[path.clone()], &old, &new, |event| {
            progress.push(event)
        })
        .unwrap();
        assert_eq!(commit.note_count, 0);
        assert_eq!(commit.recovery_count, 1);
        assert_ne!(fs::read(&path).unwrap(), before);
        let mut decrypted =
            decrypt(File::open(&path).unwrap(), &new, EnvelopeKind::Recovery).unwrap();
        let mut actual = Vec::new();
        decrypted.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, plaintext);
        assert!(decrypt(File::open(&path).unwrap(), &old, EnvelopeKind::Recovery).is_err());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(progress.iter().any(|event| {
            event.phase == PasswordChangePhase::PreparingRecovery
                && event.completed == 1
                && event.total == 1
        }));
        assert_eq!(progress.last().and_then(|event| event.percent), Some(99));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_and_hardlink_targets_are_rejected_without_writes() {
        use std::os::unix::fs::symlink;

        let root = workspace("links");
        let old = MasterPassword::new("old password".to_owned());
        let new = MasterPassword::new("new password".to_owned());
        let (path, version) = protected_note(&root, "Private.md", &old);
        let alias = root.join("notes/Alias.md");
        fs::hard_link(&path, &alias).unwrap();
        assert!(matches!(
            change_master_password(
                &root,
                &[PasswordChangeTarget {
                    path: path.clone(),
                    version,
                }],
                &[],
                &old,
                &new,
                |_| {},
            ),
            Err(PasswordChangeError::Invalid(_))
        ));
        fs::remove_file(&alias).unwrap();
        let symlink_path = root.join("notes/Symlink.md");
        symlink(&path, &symlink_path).unwrap();
        let symlink_version =
            FileVersion::from_metadata(&fs::symlink_metadata(&symlink_path).unwrap());
        assert!(matches!(
            change_master_password(
                &root,
                &[PasswordChangeTarget {
                    path: symlink_path,
                    version: symlink_version,
                }],
                &[],
                &old,
                &new,
                |_| {},
            ),
            Err(PasswordChangeError::Invalid(_))
        ));
        assert!(!root.join(".stillus_backups").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn startup_recovery_rolls_back_a_partial_commit_without_passwords() {
        let root = workspace("restart-rollback");
        let old = MasterPassword::new("old password".to_owned());
        let new = MasterPassword::new("new password".to_owned());
        let (first, first_version) = protected_note(&root, "First.md", &old);
        let (second, second_version) = protected_note(&root, "Second.md", &old);
        let first_old = fs::read(&first).unwrap();
        let second_old = fs::read(&second).unwrap();
        let snapshots = [
            preflight_note(
                &root,
                &PasswordChangeTarget {
                    path: first.clone(),
                    version: first_version,
                },
                &old,
            )
            .unwrap(),
            preflight_note(
                &root,
                &PasswordChangeTarget {
                    path: second.clone(),
                    version: second_version,
                },
                &old,
            )
            .unwrap(),
        ];
        let mut transaction = Transaction::create(&root).unwrap();
        for snapshot in &snapshots {
            transaction.prepare(snapshot, &old, &new).unwrap();
        }
        transaction.journal.state = TransactionState::Commit;
        transaction.save_journal().unwrap();
        transaction.install(0).unwrap();
        drop(transaction);

        recover_password_change(&root).unwrap();
        assert_eq!(fs::read(&first).unwrap(), first_old);
        assert_eq!(fs::read(&second).unwrap(), second_old);
        assert!(decrypted_body(&first, &old).is_ok());
        assert!(decrypted_body(&first, &new).is_err());
        assert!(active_transaction_directory(&root).unwrap().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn startup_recovery_accepts_a_fully_installed_verified_set() {
        let root = workspace("restart-commit");
        let old = MasterPassword::new("old password".to_owned());
        let new = MasterPassword::new("new password".to_owned());
        let (path, version) = protected_note(&root, "Only.md", &old);
        let snapshot = preflight_note(
            &root,
            &PasswordChangeTarget {
                path: path.clone(),
                version,
            },
            &old,
        )
        .unwrap();
        let mut transaction = Transaction::create(&root).unwrap();
        transaction.prepare(&snapshot, &old, &new).unwrap();
        transaction.journal.state = TransactionState::Commit;
        transaction.save_journal().unwrap();
        transaction.install(0).unwrap();
        drop(transaction);

        recover_password_change(&root).unwrap();
        assert!(decrypted_body(&path, &new).is_ok());
        assert!(decrypted_body(&path, &old).is_err());
        assert!(active_transaction_directory(&root).unwrap().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn startup_cleanup_is_idempotent_after_copy_removal() {
        let root = workspace("restart-cleanup");
        let old = MasterPassword::new("old password".to_owned());
        let new = MasterPassword::new("new password".to_owned());
        let (path, version) = protected_note(&root, "Only.md", &old);
        let snapshot = preflight_note(
            &root,
            &PasswordChangeTarget {
                path: path.clone(),
                version,
            },
            &old,
        )
        .unwrap();
        let mut transaction = Transaction::create(&root).unwrap();
        transaction.prepare(&snapshot, &old, &new).unwrap();
        transaction.journal.state = TransactionState::Commit;
        transaction.save_journal().unwrap();
        transaction.install(0).unwrap();
        let copy = resolve_relative(&root, &transaction.journal.entries[0].rollback_copy).unwrap();
        fs::remove_file(copy).unwrap();
        drop(transaction);

        recover_password_change(&root).unwrap();
        assert!(decrypted_body(&path, &new).is_ok());
        assert!(active_transaction_directory(&root).unwrap().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn foreign_platform_journal_is_preserved_and_blocks_recovery() {
        let root = workspace("foreign-platform-journal");
        let mut transaction = Transaction::create(&root).unwrap();
        transaction.journal.platform = if cfg!(windows) { "unix" } else { "windows" }.to_owned();
        transaction.save_journal().unwrap();
        let path = transaction.journal_path.clone();
        let before = fs::read(&path).unwrap();
        drop(transaction);
        assert!(matches!(
            recover_password_change(&root),
            Err(PasswordChangeError::Blocked(_))
        ));
        assert_eq!(fs::read(path).unwrap(), before);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tampered_journal_blocks_recovery_without_touching_artifacts() {
        let root = workspace("tampered-journal");
        let old = MasterPassword::new("old password".to_owned());
        let new = MasterPassword::new("new password".to_owned());
        let (path, version) = protected_note(&root, "Only.md", &old);
        let before = fs::read(&path).unwrap();
        let snapshot = preflight_note(
            &root,
            &PasswordChangeTarget {
                path: path.clone(),
                version,
            },
            &old,
        )
        .unwrap();
        let mut transaction = Transaction::create(&root).unwrap();
        transaction.prepare(&snapshot, &old, &new).unwrap();
        let copy = resolve_relative(&root, &transaction.journal.entries[0].rollback_copy).unwrap();
        let candidate = resolve_relative(&root, &transaction.journal.entries[0].candidate).unwrap();
        transaction.journal.entries[0].source = "README.md".to_owned();
        transaction.save_journal().unwrap();
        drop(transaction);

        assert!(matches!(
            recover_password_change(&root),
            Err(PasswordChangeError::Blocked(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(copy.exists());
        assert!(candidate.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
