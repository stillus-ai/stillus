// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use stillus_platform::fs::{self, File, OpenOptions};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{FileVersion, SaveCommit, SaveError, SaveOutcome, SaveStage, copy_bounded};

const OWNER: &[u8] = b"stillus-secure-backups-v1\n";
const MANIFEST_VERSION: u32 = 1;
const RETENTION: usize = 10;
static BACKUP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecureBackupRecord {
    pub note_id: String,
    pub sequence: u64,
    pub path: PathBuf,
    pub source_path: PathBuf,
    pub sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntegrityFailure {
    pub commit: SaveCommit,
    pub backup: SecureBackupRecord,
    pub expected_sha256: String,
    pub actual_sha256: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VerifiedSave {
    Verified(SaveCommit),
    IntegrityFailure(Box<IntegrityFailure>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    notes: Vec<NoteHistory>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct NoteHistory {
    id: String,
    current_path: String,
    next_sequence: u64,
    backups: Vec<BackupEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending: Option<PendingEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BackupEntry {
    sequence: u64,
    created_unix_ms: u64,
    path: String,
    source_path: String,
    sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PendingEntry {
    backup_sequence: u64,
    candidate_path: String,
    candidate_version: String,
    expected_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    actual_sha256: Option<String>,
    message: String,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: MANIFEST_VERSION,
            notes: Vec::new(),
        }
    }
}

pub(crate) fn prepare_backup(
    workspace: &Path,
    source: &Path,
    expected_version: &FileVersion,
) -> Result<SecureBackupRecord, SaveError> {
    let relative = backup_source_relative_path(workspace, source)?;
    let secure = ensure_store(workspace)?;
    let mut manifest = load_manifest(&secure)?;
    let relative_string = path_string(&relative)?;
    let history_index = manifest
        .notes
        .iter()
        .position(|history| history.current_path == relative_string)
        .unwrap_or_else(|| {
            let id = unique_id();
            manifest.notes.push(NoteHistory {
                id,
                current_path: relative_string.clone(),
                next_sequence: 1,
                backups: Vec::new(),
                pending: None,
            });
            manifest.notes.len() - 1
        });
    let history = &mut manifest.notes[history_index];
    if history.pending.is_some() {
        return Err(SaveError::InvalidTarget(
            "protected note has an unresolved integrity incident".to_owned(),
        ));
    }
    if let Some(entry) = history.backups.last()
        && entry.source_path == relative_string
        && hash_file_stable(source, expected_version)
            .map_err(|error| precommit(SaveStage::OpenTarget, error))?
            == entry.sha256
    {
        return Ok(SecureBackupRecord {
            note_id: history.id.clone(),
            sequence: entry.sequence,
            path: workspace.join(&entry.path),
            source_path: source.to_path_buf(),
            sha256: entry.sha256.clone(),
        });
    }
    let note_directory = secure.join(&history.id);
    if note_directory.exists() {
        ensure_existing_directory(&note_directory)?;
    } else {
        ensure_private_directory(&note_directory)?;
    }
    let sequence = history.next_sequence;
    history.next_sequence = history.next_sequence.saturating_add(1);
    let extension = source
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("bin");
    let name = format!("{sequence:020}.{extension}");
    let destination = note_directory.join(&name);
    let temporary = note_directory.join(format!(".{name}.tmp-{}", unique_id()));

    let source_metadata =
        fs::symlink_metadata(source).map_err(|error| precommit(SaveStage::OpenTarget, error))?;
    if !source_metadata.file_type().is_file()
        || FileVersion::from_metadata(&source_metadata) != *expected_version
    {
        return Err(SaveError::Conflict);
    }
    let mut input = File::open(source).map_err(|error| precommit(SaveStage::OpenTarget, error))?;
    if FileVersion::from_metadata(
        &input
            .metadata()
            .map_err(|error| precommit(SaveStage::OpenTarget, error))?,
    ) != *expected_version
    {
        return Err(SaveError::Conflict);
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(any(unix, windows))]
    {
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options
        .open(&temporary)
        .map_err(|error| precommit(SaveStage::CreateTemp, error))?;
    let copy_result = (|| -> Result<String, SaveError> {
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 65_536];
        loop {
            let read = input
                .read(&mut buffer)
                .map_err(|error| precommit(SaveStage::Write, error))?;
            if read == 0 {
                break;
            }
            output
                .write_all(&buffer[..read])
                .map_err(|error| precommit(SaveStage::Write, error))?;
            hasher.update(&buffer[..read]);
        }
        output
            .flush()
            .map_err(|error| precommit(SaveStage::Write, error))?;
        output
            .sync_all()
            .map_err(|error| precommit(SaveStage::FileSync, error))?;
        let current = fs::symlink_metadata(source)
            .map_err(|error| precommit(SaveStage::ConflictCheck, error))?;
        if !current.file_type().is_file()
            || FileVersion::from_metadata(&current) != *expected_version
        {
            return Err(SaveError::Conflict);
        }
        Ok(hex_digest(hasher.finalize().into()))
    })();
    let sha256 = match copy_result {
        Ok(hash) => hash,
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
    };
    drop(output);
    fs::rename(&temporary, &destination).map_err(|error| precommit(SaveStage::Replace, error))?;
    sync_directory(&note_directory)?;
    let copied_hash =
        hash_file(&destination).map_err(|error| precommit(SaveStage::FileSync, error))?;
    if copied_hash != sha256 {
        return Err(SaveError::InvalidTarget(
            "secure backup hash mismatch".to_owned(),
        ));
    }
    let backup_relative = destination
        .strip_prefix(workspace)
        .map_err(|_| SaveError::InvalidTarget("backup escaped workspace".to_owned()))?;
    history.backups.push(BackupEntry {
        sequence,
        created_unix_ms: unix_time_ms(),
        path: path_string(backup_relative)?,
        source_path: relative_string,
        sha256: sha256.clone(),
    });
    save_manifest(&secure, &manifest)?;
    Ok(SecureBackupRecord {
        note_id: manifest.notes[history_index].id.clone(),
        sequence,
        path: destination,
        source_path: source.to_path_buf(),
        sha256,
    })
}

pub(crate) fn verify_commit(
    workspace: &Path,
    commit: SaveCommit,
    backup: SecureBackupRecord,
    expected_sha256: String,
) -> Result<VerifiedSave, SaveError> {
    #[cfg(any(test, feature = "test-utils"))]
    let commit = {
        let mut commit = commit;
        if let Some(version) = maybe_corrupt_for_test(workspace, &commit.path) {
            commit.version = version;
        }
        commit
    };
    let actual = hash_file_stable(&commit.path, &commit.version);
    match actual {
        Ok(actual_sha256) if actual_sha256 == expected_sha256 => {
            finalize_verified(workspace, &backup, &commit.path)?;
            Ok(VerifiedSave::Verified(commit))
        }
        result => {
            let (actual_sha256, message) = match result {
                Ok(actual) => (
                    Some(actual),
                    "saved file hash does not match generated bytes".to_owned(),
                ),
                Err(error) => (None, format!("saved file could not be verified: {error}")),
            };
            mark_pending(
                workspace,
                &backup,
                &commit.path,
                &expected_sha256,
                actual_sha256.as_deref(),
                &message,
                &commit.version,
            )?;
            Ok(VerifiedSave::IntegrityFailure(Box::new(IntegrityFailure {
                commit,
                backup,
                expected_sha256,
                actual_sha256,
                message,
            })))
        }
    }
}

pub fn restore_secure_backup(
    workspace: impl AsRef<Path>,
    failure: &IntegrityFailure,
) -> Result<SaveCommit, SaveError> {
    let _operation = stillus_platform::OperationLock::directory(workspace.as_ref())
        .map_err(|error| precommit(SaveStage::OpenTarget, error))?;
    let workspace = workspace.as_ref();
    let current = fs::symlink_metadata(&failure.commit.path)
        .map_err(|error| precommit(SaveStage::OpenTarget, error))?;
    if !current.file_type().is_file()
        || FileVersion::from_metadata(&current) != failure.commit.version
    {
        return Err(SaveError::Conflict);
    }
    if hash_file(&failure.backup.path).map_err(|error| precommit(SaveStage::OpenTarget, error))?
        != failure.backup.sha256
    {
        return Err(SaveError::InvalidTarget(
            "secure rollback backup failed its hash check".to_owned(),
        ));
    }
    let destination = &failure.backup.source_path;
    let parent = destination
        .parent()
        .ok_or_else(|| SaveError::InvalidTarget("rollback path has no parent".to_owned()))?;
    let temporary = parent.join(format!(".stillus-restore-{}.tmp", unique_id()));
    let mut input = File::open(&failure.backup.path)
        .map_err(|error| precommit(SaveStage::OpenTarget, error))?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(any(unix, windows))]
    {
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options
        .open(&temporary)
        .map_err(|error| precommit(SaveStage::CreateTemp, error))?;
    if let Err(error) = copy_bounded(&mut input, &mut output)
        .and_then(|_| output.flush())
        .and_then(|_| output.sync_all())
    {
        let _ = fs::remove_file(&temporary);
        return Err(precommit(SaveStage::Write, error));
    }
    drop(output);
    if destination == &failure.commit.path {
        fs::rename(&temporary, destination)
            .map_err(|error| precommit(SaveStage::Replace, error))?;
    } else {
        if fs::symlink_metadata(destination).is_ok() {
            let _ = fs::remove_file(&temporary);
            return Err(SaveError::Conflict);
        }
        fs::rename(&temporary, destination)
            .map_err(|error| precommit(SaveStage::Replace, error))?;
        let current = fs::symlink_metadata(&failure.commit.path).map_err(|error| {
            SaveError::PartialCommit {
                path: destination.clone(),
                message: error.to_string(),
            }
        })?;
        if FileVersion::from_metadata(&current) != failure.commit.version {
            return Err(SaveError::PartialCommit {
                path: destination.clone(),
                message: "candidate changed before rollback cleanup".to_owned(),
            });
        }
        fs::remove_file(&failure.commit.path).map_err(|error| SaveError::PartialCommit {
            path: destination.clone(),
            message: error.to_string(),
        })?;
    }
    sync_directory(parent)?;
    let metadata =
        fs::symlink_metadata(destination).map_err(|error| SaveError::PostReplaceSync {
            message: error.to_string(),
        })?;
    let version = FileVersion::from_metadata(&metadata);
    let restored_hash =
        hash_file_stable(destination, &version).map_err(|error| SaveError::PostReplaceSync {
            message: error.to_string(),
        })?;
    if restored_hash != failure.backup.sha256 {
        return Err(SaveError::PostReplaceSync {
            message: "restored backup hash mismatch".to_owned(),
        });
    }
    finalize_verified(workspace, &failure.backup, destination)?;
    Ok(SaveCommit {
        outcome: SaveOutcome::Committed,
        version,
        path: destination.clone(),
    })
}

/// Loads the persisted integrity incident without exposing protected body data.
pub fn load_pending_integrity_failure(
    workspace: impl AsRef<Path>,
) -> Result<Option<IntegrityFailure>, SaveError> {
    let workspace = workspace.as_ref();
    if !workspace.join(".stillus_backups").exists() {
        return Ok(None);
    }
    let secure = ensure_store(workspace)?;
    let manifest = load_manifest(&secure)?;
    for history in manifest.notes {
        let Some(pending) = history.pending else {
            continue;
        };
        let backup = history
            .backups
            .iter()
            .find(|entry| entry.sequence == pending.backup_sequence)
            .ok_or_else(|| {
                SaveError::InvalidTarget(
                    "integrity journal references a missing secure backup".to_owned(),
                )
            })?;
        let candidate_relative = checked_relative(&pending.candidate_path)?;
        let backup_relative = checked_relative(&backup.path)?;
        let source_relative = checked_relative(&backup.source_path)?;
        let candidate = workspace.join(candidate_relative);
        let metadata = fs::symlink_metadata(&candidate)
            .map_err(|error| precommit(SaveStage::OpenTarget, error))?;
        if !metadata.file_type().is_file() {
            return Err(SaveError::Conflict);
        }
        let version = FileVersion::from_metadata(&metadata);
        if version_token(&version) != pending.candidate_version {
            return Err(SaveError::Conflict);
        }
        return Ok(Some(IntegrityFailure {
            commit: SaveCommit {
                outcome: SaveOutcome::Committed,
                version,
                path: candidate,
            },
            backup: SecureBackupRecord {
                note_id: history.id,
                sequence: backup.sequence,
                path: workspace.join(backup_relative),
                source_path: workspace.join(source_relative),
                sha256: backup.sha256.clone(),
            },
            expected_sha256: pending.expected_sha256,
            actual_sha256: pending.actual_sha256,
            message: pending.message,
        }));
    }
    Ok(None)
}

pub(crate) fn hash_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 65_536];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_digest(hasher.finalize().into()))
}

fn hash_file_stable(path: &Path, version: &FileVersion) -> io::Result<String> {
    let before = fs::symlink_metadata(path)?;
    if !before.file_type().is_file() || FileVersion::from_metadata(&before) != *version {
        return Err(io::Error::other("file changed before verification"));
    }
    let hash = hash_file(path)?;
    let after = fs::symlink_metadata(path)?;
    if FileVersion::from_metadata(&after) != *version {
        return Err(io::Error::other("file changed during verification"));
    }
    Ok(hash)
}

pub(crate) fn finalize_verified(
    workspace: &Path,
    backup: &SecureBackupRecord,
    current_path: &Path,
) -> Result<(), SaveError> {
    let secure = ensure_store(workspace)?;
    let mut manifest = load_manifest(&secure)?;
    let history = manifest
        .notes
        .iter_mut()
        .find(|history| history.id == backup.note_id)
        .ok_or_else(|| SaveError::InvalidTarget("secure backup history disappeared".to_owned()))?;
    history.current_path = path_string(&backup_source_relative_path(workspace, current_path)?)?;
    history.pending = None;
    while history.backups.len() > RETENTION {
        let entry = history.backups.remove(0);
        let path = workspace.join(&entry.path);
        if let Err(error) = fs::remove_file(&path) {
            history.backups.insert(0, entry);
            save_manifest(&secure, &manifest)?;
            return Err(SaveError::PostReplaceSync {
                message: format!("could not rotate secure backup {}: {error}", path.display()),
            });
        }
    }
    save_manifest(&secure, &manifest)
}

fn mark_pending(
    workspace: &Path,
    backup: &SecureBackupRecord,
    candidate: &Path,
    expected: &str,
    actual: Option<&str>,
    message: &str,
    version: &FileVersion,
) -> Result<(), SaveError> {
    let secure = ensure_store(workspace)?;
    let mut manifest = load_manifest(&secure)?;
    let history = manifest
        .notes
        .iter_mut()
        .find(|history| history.id == backup.note_id)
        .ok_or_else(|| SaveError::InvalidTarget("secure backup history disappeared".to_owned()))?;
    history.pending = Some(PendingEntry {
        backup_sequence: backup.sequence,
        candidate_path: path_string(&backup_source_relative_path(workspace, candidate)?)?,
        candidate_version: version_token(version),
        expected_sha256: expected.to_owned(),
        actual_sha256: actual.map(str::to_owned),
        message: message.to_owned(),
    });
    save_manifest(&secure, &manifest)
}

fn version_token(version: &FileVersion) -> String {
    format!("{version:?}")
}

fn checked_relative(value: &str) -> Result<PathBuf, SaveError> {
    let path = PathBuf::from(value);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(SaveError::InvalidTarget(
            "secure backup manifest contains an invalid path".to_owned(),
        ));
    }
    Ok(path)
}

pub(crate) fn ensure_store(workspace: &Path) -> Result<PathBuf, SaveError> {
    let root = workspace.join(".stillus_backups");
    let secure = root.join("secure");
    let marker = root.join("OWNER");
    if root.exists() {
        ensure_existing_directory(&root)?;
        let marker_metadata = fs::symlink_metadata(&marker)
            .map_err(|error| precommit(SaveStage::OpenTarget, error))?;
        if !marker_metadata.file_type().is_file() {
            return Err(SaveError::InvalidTarget(
                ".stillus_backups ownership marker must be a regular file".to_owned(),
            ));
        }
        let bytes = fs::read(&marker).map_err(|_| {
            SaveError::InvalidTarget("existing .stillus_backups is not owned by Stillus".to_owned())
        })?;
        if bytes != OWNER {
            return Err(SaveError::InvalidTarget(
                "existing .stillus_backups ownership marker is invalid".to_owned(),
            ));
        }
    } else {
        ensure_private_directory(&root)?;
        write_private_file(&marker, OWNER)?;
        sync_directory(&root)?;
    }
    if secure.exists() {
        ensure_existing_directory(&secure)?;
    } else {
        ensure_private_directory(&secure)?;
    }
    let manifest = secure.join("manifest.json");
    if !manifest.exists() {
        save_manifest(&secure, &Manifest::default())?;
    }
    Ok(secure)
}

fn ensure_private_directory(path: &Path) -> Result<(), SaveError> {
    stillus_platform::create_private_directory(path)
        .map_err(|error| precommit(SaveStage::CreateTemp, error))
}

fn ensure_existing_directory(path: &Path) -> Result<(), SaveError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| precommit(SaveStage::OpenTarget, error))?;
    if !metadata.file_type().is_dir() {
        return Err(SaveError::InvalidTarget(format!(
            "{} must be a real directory",
            path.display()
        )));
    }
    Ok(())
}

fn load_manifest(secure: &Path) -> Result<Manifest, SaveError> {
    let path = secure.join("manifest.json");
    let metadata =
        fs::symlink_metadata(&path).map_err(|error| precommit(SaveStage::OpenTarget, error))?;
    if !metadata.file_type().is_file() {
        return Err(SaveError::InvalidTarget(
            "secure backup manifest must be a regular file".to_owned(),
        ));
    }
    let bytes = fs::read(&path).map_err(|error| precommit(SaveStage::OpenTarget, error))?;
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|error| {
        SaveError::InvalidTarget(format!("invalid secure backup manifest: {error}"))
    })?;
    if manifest.version != MANIFEST_VERSION {
        return Err(SaveError::InvalidTarget(
            "unsupported secure backup manifest version".to_owned(),
        ));
    }
    Ok(manifest)
}

fn save_manifest(secure: &Path, manifest: &Manifest) -> Result<(), SaveError> {
    let bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|error| SaveError::InvalidTarget(error.to_string()))?;
    let temporary = secure.join(format!(".manifest-{}.tmp", unique_id()));
    write_private_file(&temporary, &bytes)?;
    fs::rename(&temporary, secure.join("manifest.json"))
        .map_err(|error| precommit(SaveStage::Replace, error))?;
    sync_directory(secure)
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), SaveError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(any(unix, windows))]
    {
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| precommit(SaveStage::CreateTemp, error))?;
    file.write_all(bytes)
        .and_then(|_| file.flush())
        .and_then(|_| file.sync_all())
        .map_err(|error| precommit(SaveStage::FileSync, error))
}

fn sync_directory(path: &Path) -> Result<(), SaveError> {
    stillus_platform::sync_directory(path).map_err(|error| SaveError::PostReplaceSync {
        message: error.to_string(),
    })
}

fn backup_source_relative_path(workspace: &Path, path: &Path) -> Result<PathBuf, SaveError> {
    let relative = path
        .strip_prefix(workspace)
        .map_err(|_| SaveError::InvalidTarget("backup source is outside workspace".to_owned()))?;
    let components = relative.components().collect::<Vec<_>>();
    let direct_note = matches!(
        components.as_slice(),
        [Component::Normal(root), Component::Normal(_)] if root == &"notes"
    );
    let engine_secret = matches!(
        components.as_slice(),
        [Component::Normal(root), Component::Normal(directory), Component::Normal(file)]
            if root == &".stillus_security"
                && directory == &"secrets"
                && file
                    .to_str()
                    .and_then(|name| name.strip_suffix(".age"))
                    .is_some_and(|id| id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
    );
    if !direct_note && !engine_secret {
        return Err(SaveError::InvalidTarget(
            "secure backup source must be a direct note or engine secret".to_owned(),
        ));
    }
    Ok(relative.to_path_buf())
}

fn path_string(path: &Path) -> Result<String, SaveError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| SaveError::InvalidTarget("backup path must be UTF-8".to_owned()))
}

fn unique_id() -> String {
    format!(
        "{}-{}-{}",
        std::process::id(),
        BACKUP_ID.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    )
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn hex_digest(bytes: [u8; 32]) -> String {
    let mut output = String::with_capacity(64);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn precommit(stage: SaveStage, error: impl std::fmt::Display) -> SaveError {
    SaveError::PreCommit {
        stage,
        message: error.to_string(),
    }
}

#[cfg(any(test, feature = "test-utils"))]
fn maybe_corrupt_for_test(workspace: &Path, path: &Path) -> Option<FileVersion> {
    let trigger = workspace.join(".stillus/test-corrupt-protected-save");
    let action = fs::read(&trigger).ok()?;
    if fs::remove_file(&trigger).is_err() {
        return None;
    }
    if action == b"remove" {
        let _ = fs::remove_file(path);
        return None;
    }
    if let Ok(mut file) = OpenOptions::new().append(true).open(path) {
        let _ = file.write_all(b"stillus-integrity-fault");
        let _ = file.sync_all();
    }
    fs::symlink_metadata(path)
        .ok()
        .map(|metadata| FileVersion::from_metadata(&metadata))
}
