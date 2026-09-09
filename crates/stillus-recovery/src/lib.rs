// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use stillus_platform::fs::{self, File, OpenOptions};

use stillus_secure::{
    AGE_PREFIX, EnvelopeKind, EnvelopeMetadata, EnvelopeReader, EnvelopeWriter, MasterPassword,
    decrypt, is_age_prefix,
};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

const MAGIC: &[u8] = b"STILLUSRECOVERY1\n";
const FIXED_HEADER_BYTES: u64 = MAGIC.len() as u64 + 4 + (4 * 8);
const MAX_PATH_BYTES: usize = 4_096;
const FNV_OFFSET_1: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_OFFSET_2: u64 = 0x8422_2325_cbf2_9ce4;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
const PROTECTED_TEMP_STALE_AFTER: std::time::Duration =
    std::time::Duration::from_secs(24 * 60 * 60);
const MAX_AGE_HEADER_BYTES: usize = 8 * 1024;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct RecoveryStore {
    workspace: PathBuf,
    observed: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<String, (RecoveryRecord, ArtifactVersion)>>,
    >,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryKey {
    relative_path: String,
    artifact_name: String,
}

impl RecoveryKey {
    pub fn relative_path(&self) -> &str {
        &self.relative_path
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryRecord {
    pub key: RecoveryKey,
    pub revision: u64,
    pub base_checksum: u64,
    pub body_len: u64,
    pub body_checksum: u64,
}

pub struct RecoveryArtifact {
    pub record: RecoveryRecord,
    pub body: RecoveryBody,
}

pub struct RecoveryBody {
    inner: RecoveryBodyInner,
}

enum RecoveryBodyInner {
    Plain(File),
    Protected(Box<EnvelopeReader<File>>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ExistingArtifact {
    Absent,
    Current(RecoveryRecord),
    Quarantined,
}

enum ExpectedArtifact<'a> {
    Plain,
    Protected(&'a MasterPassword),
}

enum ArtifactInspection {
    Current(RecoveryRecord),
    Collision,
    Unusable,
}

impl RecoveryBody {
    fn plain(file: File) -> Self {
        Self {
            inner: RecoveryBodyInner::Plain(file),
        }
    }

    fn protected(reader: EnvelopeReader<File>) -> Self {
        Self {
            inner: RecoveryBodyInner::Protected(Box::new(reader)),
        }
    }
}

impl Read for RecoveryBody {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match &mut self.inner {
            RecoveryBodyInner::Plain(file) => file.read(buffer),
            RecoveryBodyInner::Protected(reader) => reader.read(buffer),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RecoveryScan {
    pub records: Vec<RecoveryRecord>,
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProtectedRecoveryRecord {
    pub artifact_name: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProtectedRecoveryScan {
    pub records: Vec<ProtectedRecoveryRecord>,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum RecoveryError {
    UnsupportedPlatform,
    InvalidPath(String),
    InvalidStore(String),
    InvalidArtifact(String),
    Io(String),
}

impl std::fmt::Display for RecoveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                formatter.write_str("atomic recovery write is not implemented on this platform")
            }
            Self::InvalidPath(message) => write!(formatter, "invalid recovery path: {message}"),
            Self::InvalidStore(message) => write!(formatter, "invalid recovery store: {message}"),
            Self::InvalidArtifact(message) => {
                write!(formatter, "invalid recovery artifact: {message}")
            }
            Self::Io(message) => write!(formatter, "recovery I/O error: {message}"),
        }
    }
}

impl std::error::Error for RecoveryError {}

impl RecoveryStore {
    pub fn new(workspace: impl AsRef<Path>) -> Self {
        Self {
            workspace: workspace.as_ref().to_path_buf(),
            observed: Default::default(),
        }
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn key_for_note(&self, note: impl AsRef<Path>) -> Result<RecoveryKey, RecoveryError> {
        let relative = note
            .as_ref()
            .strip_prefix(&self.workspace)
            .map_err(|_| RecoveryError::InvalidPath("note is outside workspace".to_owned()))?;
        if relative.as_os_str().is_empty()
            || relative.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(RecoveryError::InvalidPath(
                "relative note path is not safe".to_owned(),
            ));
        }
        let relative_path = relative
            .to_str()
            .ok_or_else(|| RecoveryError::InvalidPath("path is not valid UTF-8".to_owned()))?
            .replace(std::path::MAIN_SEPARATOR, "/");
        if relative_path.len() > MAX_PATH_BYTES {
            return Err(RecoveryError::InvalidPath("path is too long".to_owned()));
        }
        Ok(key_from_relative(relative_path))
    }

    pub fn key_for_external(
        &self,
        engine_id: &str,
        item_id: &str,
    ) -> Result<RecoveryKey, RecoveryError> {
        if engine_id.is_empty()
            || item_id.is_empty()
            || engine_id.contains("..")
            || item_id.contains("..")
            || engine_id.contains('\\')
            || item_id.contains('\\')
        {
            return Err(RecoveryError::InvalidPath(
                "external recovery identity is not safe".to_owned(),
            ));
        }
        let relative_path = format!("external/{engine_id}/{item_id}");
        if relative_path.len() > MAX_PATH_BYTES {
            return Err(RecoveryError::InvalidPath(
                "external recovery identity is too long".to_owned(),
            ));
        }
        Ok(key_from_relative(relative_path))
    }

    pub fn scan(&self) -> RecoveryScan {
        let directory = match self.checked_directory() {
            Ok(Some(directory)) => directory,
            Ok(None) => return RecoveryScan::default(),
            Err(error) => {
                return RecoveryScan {
                    records: Vec::new(),
                    diagnostics: vec![error.to_string()],
                };
            }
        };
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return RecoveryScan::default();
            }
            Err(error) => {
                return RecoveryScan {
                    records: Vec::new(),
                    diagnostics: vec![error.to_string()],
                };
            }
        };
        let mut scan = RecoveryScan::default();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    scan.diagnostics.push(error.to_string());
                    continue;
                }
            };
            if is_quarantine_artifact_name(&entry.file_name()) {
                scan.diagnostics.push(format!(
                    "unusable recovery artifact was quarantined and retained at {}",
                    entry.path().display()
                ));
                continue;
            }
            if entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "nrrec")
            {
                continue;
            }
            let result = (|| {
                if !entry.file_type().map_err(io_error)?.is_file() {
                    return Err(RecoveryError::InvalidArtifact(
                        "artifact is not a regular file".to_owned(),
                    ));
                }
                let mut file = File::open(entry.path()).map_err(io_error)?;
                if has_age_prefix(&mut file)? {
                    return Ok(None);
                }
                let record = read_header(&mut file)?;
                if entry.file_name() != OsString::from(&record.key.artifact_name) {
                    return Err(RecoveryError::InvalidArtifact(
                        "artifact name does not match path key".to_owned(),
                    ));
                }
                Ok(Some(record))
            })();
            match result {
                Ok(Some(record)) => scan.records.push(record),
                Ok(None) => {}
                Err(error) => scan
                    .diagnostics
                    .push(format!("{}: {error}", entry.path().display())),
            }
        }
        scan.records
            .sort_by(|left, right| left.key.relative_path.cmp(&right.key.relative_path));
        scan
    }

    pub fn open(&self, key: &RecoveryKey) -> Result<RecoveryArtifact, RecoveryError> {
        let _operation =
            stillus_platform::OperationLock::directory(&self.workspace).map_err(io_error)?;
        let path = self.require_directory()?.join(&key.artifact_name);
        let metadata = fs::symlink_metadata(&path).map_err(io_error)?;
        if !metadata.file_type().is_file() {
            return Err(RecoveryError::InvalidArtifact(
                "artifact must be a regular file".to_owned(),
            ));
        }
        let mut body = File::open(path).map_err(io_error)?;
        let record = read_header(&mut body)?;
        if record.key != *key {
            return Err(RecoveryError::InvalidArtifact(
                "artifact path key mismatch".to_owned(),
            ));
        }
        self.remember(&record)?;
        Ok(RecoveryArtifact {
            record,
            body: RecoveryBody::plain(body),
        })
    }

    pub fn scan_protected(&self) -> ProtectedRecoveryScan {
        let mut scan = ProtectedRecoveryScan::default();
        let directory = match self.checked_directory() {
            Ok(Some(directory)) => directory,
            Ok(None) => return scan,
            Err(error) => {
                scan.diagnostics.push(error.to_string());
                return scan;
            }
        };
        if let Err(error) = cleanup_protected_temps(&directory) {
            scan.diagnostics.push(error.to_string());
        }
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return scan,
            Err(error) => {
                scan.diagnostics.push(error.to_string());
                return scan;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    scan.diagnostics.push(error.to_string());
                    continue;
                }
            };
            if is_quarantine_artifact_name(&entry.file_name()) {
                continue;
            }
            if entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "nrrec")
            {
                continue;
            }
            let result = (|| {
                if !entry.file_type().map_err(io_error)?.is_file() {
                    return Err(RecoveryError::InvalidArtifact(
                        "artifact is not a regular file".to_owned(),
                    ));
                }
                let mut file = File::open(entry.path()).map_err(io_error)?;
                has_age_prefix(&mut file)
            })();
            match result {
                Ok(true) => scan.records.push(ProtectedRecoveryRecord {
                    artifact_name: entry.file_name().to_string_lossy().into_owned(),
                }),
                Ok(false) => {}
                Err(error) => scan
                    .diagnostics
                    .push(format!("{}: {error}", entry.path().display())),
            }
        }
        scan.records
            .sort_by(|left, right| left.artifact_name.cmp(&right.artifact_name));
        scan
    }

    /// Returns the active encrypted recovery artifacts. Quarantine files and
    /// unrelated entries are deliberately excluded.
    pub fn protected_artifact_paths(&self) -> Result<Vec<PathBuf>, RecoveryError> {
        let scan = self.scan_protected();
        if !scan.diagnostics.is_empty() {
            return Err(RecoveryError::InvalidArtifact(
                "encrypted recovery artifacts could not be enumerated safely".to_owned(),
            ));
        }
        let Some(directory) = self.checked_directory()? else {
            return Ok(Vec::new());
        };
        Ok(scan
            .records
            .into_iter()
            .map(|record| directory.join(record.artifact_name))
            .collect())
    }

    pub fn validate_protected_artifact(
        &self,
        path: impl AsRef<Path>,
        password: &MasterPassword,
    ) -> Result<RecoveryRecord, RecoveryError> {
        let path = path.as_ref();
        let directory = self.require_directory()?;
        if path.parent() != Some(directory.as_path()) {
            return Err(RecoveryError::InvalidPath(
                "recovery artifact is outside the recovery directory".to_owned(),
            ));
        }
        let artifact_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(protected_failure)?;
        let metadata = fs::symlink_metadata(path).map_err(protected_error)?;
        if !metadata.file_type().is_file() {
            return Err(protected_failure());
        }
        validate_protected_payload_for_name(
            File::open(path).map_err(protected_error)?,
            artifact_name,
            password,
        )
    }

    pub fn protected_exists(&self, key: &RecoveryKey) -> Result<bool, RecoveryError> {
        let Some(directory) = self.checked_directory()? else {
            return Ok(false);
        };
        let path = directory.join(&key.artifact_name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(io_error(error)),
        };
        if !metadata.file_type().is_file() {
            return Err(RecoveryError::InvalidArtifact(
                "artifact must be a regular file".to_owned(),
            ));
        }
        let mut file = File::open(path).map_err(io_error)?;
        has_age_prefix(&mut file)
    }

    pub fn open_protected(
        &self,
        key: &RecoveryKey,
        password: &MasterPassword,
    ) -> Result<RecoveryArtifact, RecoveryError> {
        let _operation =
            stillus_platform::OperationLock::directory(&self.workspace).map_err(io_error)?;
        let path = self.require_directory()?.join(&key.artifact_name);
        let expected = protected_artifact_version(&path)?;
        let validation = File::open(&path).map_err(protected_error)?;
        let record = validate_protected_payload(validation, key, password)?;
        if protected_artifact_version(&path)? != expected {
            return Err(protected_failure());
        }

        let body_file = File::open(&path).map_err(protected_error)?;
        if ArtifactVersion::from_metadata(&body_file.metadata().map_err(protected_error)?)
            != expected
        {
            return Err(protected_failure());
        }
        let mut reader =
            decrypt(body_file, password, EnvelopeKind::Recovery).map_err(protected_error)?;
        validate_envelope_metadata(reader.metadata(), key)?;
        let reopened_record = read_header(&mut reader).map_err(|_| protected_failure())?;
        if reopened_record != record || reopened_record.key != *key {
            return Err(protected_failure());
        }
        self.remember(&record)?;
        Ok(RecoveryArtifact {
            record,
            body: RecoveryBody::protected(reader),
        })
    }

    #[cfg(any(unix, windows))]
    pub fn write(
        &self,
        key: &RecoveryKey,
        revision: u64,
        base_checksum: u64,
        body_len: u64,
        body_checksum: u64,
        write_body: impl FnOnce(&mut dyn Write) -> io::Result<()>,
    ) -> Result<RecoveryRecord, RecoveryError> {
        let _operation =
            stillus_platform::OperationLock::directory(&self.workspace).map_err(io_error)?;
        let directory = self.ensure_directory()?;
        let existing = self.prepare_existing(&directory, key, ExpectedArtifact::Plain)?;
        self.check_observed(&existing)?;
        let (mut temp, mut guard) = create_temp(&directory, key)?;
        write_header(
            &mut temp,
            key,
            revision,
            base_checksum,
            body_len,
            body_checksum,
        )?;
        let (written, hash) = {
            let mut hashing = HashingWriter::new(&mut temp);
            write_body(&mut hashing).map_err(io_error)?;
            (hashing.written, hashing.hash)
        };
        if written != body_len || hash != body_checksum {
            return Err(RecoveryError::InvalidArtifact(format!(
                "body mismatch: expected {body_len}/{body_checksum:016x}, got {written}/{hash:016x}"
            )));
        }
        temp.flush().map_err(io_error)?;
        temp.sync_all().map_err(io_error)?;
        drop(temp);
        fs::rename(guard.path(), directory.join(&key.artifact_name)).map_err(io_error)?;
        guard.disarm();
        stillus_platform::sync_directory(&directory).map_err(io_error)?;
        let record = RecoveryRecord {
            key: key.clone(),
            revision,
            base_checksum,
            body_len,
            body_checksum,
        };
        self.remember(&record)?;
        Ok(record)
    }

    #[cfg(any(unix, windows))]
    #[allow(clippy::too_many_arguments)]
    pub fn write_protected(
        &self,
        key: &RecoveryKey,
        password: &MasterPassword,
        revision: u64,
        base_checksum: u64,
        body_len: u64,
        body_checksum: u64,
        write_body: impl FnOnce(&mut dyn Write) -> io::Result<()>,
    ) -> Result<RecoveryRecord, RecoveryError> {
        let _operation =
            stillus_platform::OperationLock::directory(&self.workspace).map_err(io_error)?;
        let directory = self.ensure_directory()?;
        let final_path = directory.join(&key.artifact_name);
        let existing =
            self.prepare_existing(&directory, key, ExpectedArtifact::Protected(password))?;
        self.check_observed(&existing)?;

        let payload_len = recovery_payload_len(key, body_len)?;
        let metadata = EnvelopeMetadata::new(
            EnvelopeKind::Recovery,
            key.artifact_name.clone(),
            payload_len,
        )
        .map_err(protected_error)?;
        let (temp, mut guard) = create_protected_temp(&directory, key)?;
        let mut encrypted =
            create_envelope_writer(temp, password, metadata).map_err(protected_error)?;
        write_header(
            &mut encrypted,
            key,
            revision,
            base_checksum,
            body_len,
            body_checksum,
        )?;
        let (written, hash) = {
            let mut hashing = HashingWriter::new(&mut encrypted);
            write_body(&mut hashing).map_err(io_error)?;
            (hashing.written, hashing.hash)
        };
        if written != body_len || hash != body_checksum {
            return Err(RecoveryError::InvalidArtifact(format!(
                "body mismatch: expected {body_len}/{body_checksum:016x}, got {written}/{hash:016x}"
            )));
        }
        let mut temp = encrypted.finish().map_err(protected_error)?;
        temp.flush().map_err(protected_error)?;
        temp.sync_all().map_err(protected_error)?;
        drop(temp);
        fs::rename(guard.path(), &final_path).map_err(protected_error)?;
        guard.disarm();
        sync_recovery_directory(&directory)?;
        let record = RecoveryRecord {
            key: key.clone(),
            revision,
            base_checksum,
            body_len,
            body_checksum,
        };
        self.remember(&record)?;
        Ok(record)
    }

    #[cfg(not(any(unix, windows)))]
    #[allow(clippy::too_many_arguments)]
    pub fn write_protected(
        &self,
        _key: &RecoveryKey,
        _password: &MasterPassword,
        _revision: u64,
        _base_checksum: u64,
        _body_len: u64,
        _body_checksum: u64,
        _write_body: impl FnOnce(&mut dyn Write) -> io::Result<()>,
    ) -> Result<RecoveryRecord, RecoveryError> {
        Err(RecoveryError::UnsupportedPlatform)
    }

    #[cfg(not(any(unix, windows)))]
    pub fn write(
        &self,
        _key: &RecoveryKey,
        _revision: u64,
        _base_checksum: u64,
        _body_len: u64,
        _body_checksum: u64,
        _write_body: impl FnOnce(&mut dyn Write) -> io::Result<()>,
    ) -> Result<RecoveryRecord, RecoveryError> {
        Err(RecoveryError::UnsupportedPlatform)
    }

    pub fn remove_saved(
        &self,
        key: &RecoveryKey,
        saved_revision: u64,
    ) -> Result<bool, RecoveryError> {
        let _operation =
            stillus_platform::OperationLock::directory(&self.workspace).map_err(io_error)?;
        let Some(directory) = self.checked_directory()? else {
            return Ok(false);
        };
        let path = directory.join(&key.artifact_name);
        let artifact = self.prepare_existing(&directory, key, ExpectedArtifact::Plain)?;
        let ExistingArtifact::Current(record) = artifact else {
            return Ok(matches!(artifact, ExistingArtifact::Quarantined));
        };
        if record.revision > saved_revision
            || self
                .check_observed(&ExistingArtifact::Current(record.clone()))
                .is_err()
        {
            return Ok(false);
        }
        fs::remove_file(path).map_err(io_error)?;
        sync_recovery_directory(&directory)?;
        Ok(true)
    }

    pub fn remove(&self, key: &RecoveryKey) -> Result<bool, RecoveryError> {
        let _operation =
            stillus_platform::OperationLock::directory(&self.workspace).map_err(io_error)?;
        let Some(directory) = self.checked_directory()? else {
            return Ok(false);
        };
        let path = directory.join(&key.artifact_name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if !metadata.file_type().is_file() => {
                return Err(RecoveryError::InvalidArtifact(
                    "artifact must be a regular file".to_owned(),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(io_error(error)),
        }
        let version =
            ArtifactVersion::from_metadata(&fs::symlink_metadata(&path).map_err(io_error)?);
        let observed = self
            .observed
            .lock()
            .map_err(|_| RecoveryError::Io("recovery state lock failed".to_owned()))?;
        if observed.get(&key.artifact_name).map(|(_, version)| version) != Some(&version) {
            return Err(RecoveryError::InvalidArtifact(
                "recovery changed in another window; existing unsaved work was preserved"
                    .to_owned(),
            ));
        }
        drop(observed);
        fs::remove_file(path).map_err(io_error)?;
        sync_recovery_directory(&directory)?;
        Ok(true)
    }

    pub fn remove_protected(&self, key: &RecoveryKey) -> Result<bool, RecoveryError> {
        let _operation =
            stillus_platform::OperationLock::directory(&self.workspace).map_err(io_error)?;
        if !self.protected_exists(key)? {
            return Ok(false);
        }
        self.remove(key)
    }

    pub fn remove_protected_saved(
        &self,
        key: &RecoveryKey,
        password: &MasterPassword,
        saved_revision: u64,
    ) -> Result<bool, RecoveryError> {
        let _operation =
            stillus_platform::OperationLock::directory(&self.workspace).map_err(io_error)?;
        let Some(directory) = self.checked_directory()? else {
            return Ok(false);
        };
        let artifact =
            self.prepare_existing(&directory, key, ExpectedArtifact::Protected(password))?;
        let ExistingArtifact::Current(record) = artifact else {
            return Ok(matches!(artifact, ExistingArtifact::Quarantined));
        };
        if record.revision > saved_revision
            || self
                .check_observed(&ExistingArtifact::Current(record.clone()))
                .is_err()
        {
            return Ok(false);
        }
        self.remember(&record)?;
        self.remove_protected(key)
    }

    fn remember(&self, record: &RecoveryRecord) -> Result<(), RecoveryError> {
        let path = self.require_directory()?.join(&record.key.artifact_name);
        let version =
            ArtifactVersion::from_metadata(&fs::symlink_metadata(path).map_err(io_error)?);
        self.observed
            .lock()
            .map_err(|_| RecoveryError::Io("recovery state lock failed".to_owned()))?
            .insert(record.key.artifact_name.clone(), (record.clone(), version));
        Ok(())
    }

    fn check_observed(&self, existing: &ExistingArtifact) -> Result<(), RecoveryError> {
        if let ExistingArtifact::Current(record) = existing {
            let observed = self
                .observed
                .lock()
                .map_err(|_| RecoveryError::Io("recovery state lock failed".to_owned()))?;
            if observed
                .get(&record.key.artifact_name)
                .map(|(record, _)| record)
                != Some(record)
            {
                return Err(RecoveryError::InvalidArtifact(
                    "recovery changed in another window; existing unsaved work was preserved"
                        .to_owned(),
                ));
            }
        }
        Ok(())
    }

    fn prepare_existing(
        &self,
        directory: &Path,
        key: &RecoveryKey,
        expected: ExpectedArtifact<'_>,
    ) -> Result<ExistingArtifact, RecoveryError> {
        let path = directory.join(&key.artifact_name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(ExistingArtifact::Absent);
            }
            Err(error) => return Err(io_error(error)),
        };
        if !metadata.file_type().is_file() {
            return Err(RecoveryError::InvalidArtifact(
                "artifact must be a regular file".to_owned(),
            ));
        }
        let version = ArtifactVersion::from_metadata(&metadata);
        let mut file = File::open(&path).map_err(io_error)?;
        let opened = file.metadata().map_err(io_error)?;
        if !opened.file_type().is_file() || ArtifactVersion::from_metadata(&opened) != version {
            return Err(RecoveryError::InvalidArtifact(
                "artifact changed while being inspected".to_owned(),
            ));
        }

        let inspection = inspect_artifact(&mut file, key, &expected);
        drop(file);
        match inspection {
            ArtifactInspection::Current(record) => Ok(ExistingArtifact::Current(record)),
            ArtifactInspection::Collision => Err(RecoveryError::InvalidArtifact(
                "hash collision with another note".to_owned(),
            )),
            ArtifactInspection::Unusable => {
                match expected {
                    ExpectedArtifact::Plain => {
                        quarantine_plain_artifact(directory, key, &version)?;
                    }
                    ExpectedArtifact::Protected(password) => {
                        quarantine_protected_artifact(directory, key, &version, password)?;
                    }
                }
                Ok(ExistingArtifact::Quarantined)
            }
        }
    }

    fn checked_directory(&self) -> Result<Option<PathBuf>, RecoveryError> {
        let hidden = self.workspace.join(".stillus");
        if !existing_real_directory(&hidden)? {
            return Ok(None);
        }
        let recovery = hidden.join("recovery");
        if !existing_real_directory(&recovery)? {
            return Ok(None);
        }
        Ok(Some(recovery))
    }

    fn require_directory(&self) -> Result<PathBuf, RecoveryError> {
        self.checked_directory()?.ok_or_else(|| {
            RecoveryError::InvalidStore("recovery directory is unavailable".to_owned())
        })
    }

    fn ensure_directory(&self) -> Result<PathBuf, RecoveryError> {
        let hidden = self.workspace.join(".stillus");
        ensure_real_directory(&hidden)?;
        let recovery = hidden.join("recovery");
        ensure_real_directory(&recovery)?;
        Ok(recovery)
    }
}

fn existing_real_directory(path: &Path) -> Result<bool, RecoveryError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(true),
        Ok(_) => Err(RecoveryError::InvalidStore(format!(
            "{} is not a real directory",
            path.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error(error)),
    }
}

fn ensure_real_directory(path: &Path) -> Result<(), RecoveryError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(RecoveryError::InvalidStore(format!(
            "{} is not a real directory",
            path.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(io_error)?;
            let metadata = fs::symlink_metadata(path).map_err(io_error)?;
            if metadata.file_type().is_dir() {
                Ok(())
            } else {
                Err(RecoveryError::InvalidStore(format!(
                    "{} changed while being created",
                    path.display()
                )))
            }
        }
        Err(error) => Err(io_error(error)),
    }
}

fn key_from_relative(relative_path: String) -> RecoveryKey {
    let first = fnv(FNV_OFFSET_1, relative_path.as_bytes());
    let second = fnv(FNV_OFFSET_2, relative_path.as_bytes());
    RecoveryKey {
        relative_path,
        artifact_name: format!("{first:016x}{second:016x}.nrrec"),
    }
}

fn inspect_artifact(
    file: &mut File,
    key: &RecoveryKey,
    expected: &ExpectedArtifact<'_>,
) -> ArtifactInspection {
    let protected = match has_age_prefix(file) {
        Ok(protected) => protected,
        Err(_) => return ArtifactInspection::Unusable,
    };
    if protected {
        let ExpectedArtifact::Protected(password) = expected else {
            return ArtifactInspection::Unusable;
        };
        let reopened = match file.try_clone() {
            Ok(reopened) => reopened,
            Err(_) => return ArtifactInspection::Unusable,
        };
        return match validate_protected_payload_for_name(reopened, &key.artifact_name, password) {
            Ok(record) if record.key == *key => ArtifactInspection::Current(record),
            Ok(_) => ArtifactInspection::Collision,
            Err(_) => ArtifactInspection::Unusable,
        };
    }

    match read_header(file) {
        Ok(record) if record.key == *key && matches!(expected, ExpectedArtifact::Plain) => {
            ArtifactInspection::Current(record)
        }
        Ok(record) if record.key != *key => ArtifactInspection::Collision,
        Ok(_) | Err(_) => ArtifactInspection::Unusable,
    }
}

fn write_header(
    writer: &mut impl Write,
    key: &RecoveryKey,
    revision: u64,
    base_checksum: u64,
    body_len: u64,
    body_checksum: u64,
) -> Result<(), RecoveryError> {
    let path = key.relative_path.as_bytes();
    let path_len = u32::try_from(path.len())
        .map_err(|_| RecoveryError::InvalidPath("path is too long".to_owned()))?;
    writer.write_all(MAGIC).map_err(io_error)?;
    writer
        .write_all(&path_len.to_le_bytes())
        .map_err(io_error)?;
    writer
        .write_all(&revision.to_le_bytes())
        .map_err(io_error)?;
    writer
        .write_all(&base_checksum.to_le_bytes())
        .map_err(io_error)?;
    writer
        .write_all(&body_len.to_le_bytes())
        .map_err(io_error)?;
    writer
        .write_all(&body_checksum.to_le_bytes())
        .map_err(io_error)?;
    writer.write_all(path).map_err(io_error)
}

fn read_header(reader: &mut impl Read) -> Result<RecoveryRecord, RecoveryError> {
    let mut magic = [0_u8; MAGIC.len()];
    reader.read_exact(&mut magic).map_err(io_error)?;
    if magic != MAGIC {
        return Err(RecoveryError::InvalidArtifact("bad magic".to_owned()));
    }
    let path_len = read_u32(reader)? as usize;
    if path_len == 0 || path_len > MAX_PATH_BYTES {
        return Err(RecoveryError::InvalidArtifact(
            "invalid path length".to_owned(),
        ));
    }
    let revision = read_u64(reader)?;
    let base_checksum = read_u64(reader)?;
    let body_len = read_u64(reader)?;
    let body_checksum = read_u64(reader)?;
    let mut path = vec![0_u8; path_len];
    reader.read_exact(&mut path).map_err(io_error)?;
    let relative_path = String::from_utf8(path)
        .map_err(|_| RecoveryError::InvalidArtifact("path is not UTF-8".to_owned()))?;
    let key = key_from_relative(relative_path);
    Ok(RecoveryRecord {
        key,
        revision,
        base_checksum,
        body_len,
        body_checksum,
    })
}

fn read_u32(reader: &mut impl Read) -> Result<u32, RecoveryError> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes).map_err(io_error)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(reader: &mut impl Read) -> Result<u64, RecoveryError> {
    let mut bytes = [0_u8; 8];
    reader.read_exact(&mut bytes).map_err(io_error)?;
    Ok(u64::from_le_bytes(bytes))
}

fn recovery_payload_len(key: &RecoveryKey, body_len: u64) -> Result<u64, RecoveryError> {
    let path_len = u64::try_from(key.relative_path.len())
        .map_err(|_| RecoveryError::InvalidPath("path is too long".to_owned()))?;
    FIXED_HEADER_BYTES
        .checked_add(path_len)
        .and_then(|header_len| header_len.checked_add(body_len))
        .ok_or_else(protected_failure)
}

fn validate_envelope_metadata(
    metadata: &EnvelopeMetadata,
    key: &RecoveryKey,
) -> Result<(), RecoveryError> {
    if metadata.kind != EnvelopeKind::Recovery || metadata.original_filename != key.artifact_name {
        return Err(protected_failure());
    }
    Ok(())
}

fn validate_protected_payload(
    file: File,
    key: &RecoveryKey,
    password: &MasterPassword,
) -> Result<RecoveryRecord, RecoveryError> {
    let record = validate_protected_payload_for_name(file, &key.artifact_name, password)?;
    if record.key != *key {
        return Err(protected_failure());
    }
    Ok(record)
}

fn validate_protected_payload_for_name(
    file: File,
    artifact_name: &str,
    password: &MasterPassword,
) -> Result<RecoveryRecord, RecoveryError> {
    let mut reader = decrypt(file, password, EnvelopeKind::Recovery).map_err(protected_error)?;
    if reader.metadata().kind != EnvelopeKind::Recovery
        || reader.metadata().original_filename != artifact_name
    {
        return Err(protected_failure());
    }
    let envelope_payload_len = reader.metadata().payload_len;
    let record = read_header(&mut reader).map_err(|_| protected_failure())?;
    if recovery_payload_len(&record.key, record.body_len)? != envelope_payload_len {
        return Err(protected_failure());
    }

    let mut remaining = record.body_len;
    let mut checksum = FNV_OFFSET_1;
    let mut buffer = [0_u8; 65_536];
    while remaining > 0 {
        let requested = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        let read = reader
            .read(&mut buffer[..requested])
            .map_err(protected_error)?;
        if read == 0 {
            return Err(protected_failure());
        }
        checksum = fnv(checksum, &buffer[..read]);
        remaining -= read as u64;
    }
    let mut trailing = [0_u8; 1];
    if reader.read(&mut trailing).map_err(protected_error)? != 0 || checksum != record.body_checksum
    {
        return Err(protected_failure());
    }
    Ok(record)
}

fn quarantine_artifact_name(key: &RecoveryKey) -> String {
    let hash = key
        .artifact_name
        .strip_suffix(".nrrec")
        .unwrap_or(&key.artifact_name);
    let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
    format!(".quarantine-{hash}-{}-{id}.nrrec", std::process::id())
}

fn is_quarantine_artifact_name(value: &OsString) -> bool {
    let Some(value) = value.to_str() else {
        return false;
    };
    let Some(value) = value
        .strip_prefix(".quarantine-")
        .and_then(|value| value.strip_suffix(".nrrec"))
    else {
        return false;
    };
    let mut parts = value.split('-');
    let (Some(hash), Some(process_id), Some(temp_id), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    hash.len() == 32
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        && process_id.parse::<u32>().is_ok_and(|value| value != 0)
        && temp_id.parse::<u64>().is_ok()
        && is_canonical_decimal(process_id)
        && is_canonical_decimal(temp_id)
}

fn quarantine_plain_artifact(
    directory: &Path,
    key: &RecoveryKey,
    expected: &ArtifactVersion,
) -> Result<(), RecoveryError> {
    let source = directory.join(&key.artifact_name);
    let before_link = fs::symlink_metadata(&source).map_err(io_error)?;
    if !before_link.file_type().is_file()
        || ArtifactVersion::from_metadata(&before_link) != *expected
    {
        return Err(RecoveryError::InvalidArtifact(
            "artifact changed while being quarantined".to_owned(),
        ));
    }
    for _ in 0..32 {
        let destination = directory.join(quarantine_artifact_name(key));
        match fs::hard_link(&source, &destination) {
            Ok(()) => {
                // Linking bumps the inode change time of the source itself, so
                // the post-link check compares everything except that clock.
                let destination_metadata = fs::symlink_metadata(&destination).map_err(io_error)?;
                let current = fs::symlink_metadata(&source).map_err(io_error)?;
                if !destination_metadata.file_type().is_file()
                    || !ArtifactVersion::from_metadata(&destination_metadata)
                        .same_linked_content(expected)
                    || !ArtifactVersion::from_metadata(&current).same_linked_content(expected)
                {
                    let _ = fs::remove_file(&destination);
                    return Err(RecoveryError::InvalidArtifact(
                        "artifact changed while being quarantined".to_owned(),
                    ));
                }
                fs::remove_file(&source).map_err(io_error)?;
                sync_recovery_directory(directory)?;
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error(error)),
        }
    }
    Err(RecoveryError::Io(
        "could not allocate recovery quarantine file".to_owned(),
    ))
}

fn quarantine_protected_artifact(
    directory: &Path,
    key: &RecoveryKey,
    expected: &ArtifactVersion,
    password: &MasterPassword,
) -> Result<(), RecoveryError> {
    let source_path = directory.join(&key.artifact_name);
    let mut source = File::open(&source_path).map_err(protected_error)?;
    if ArtifactVersion::from_metadata(&source.metadata().map_err(protected_error)?) != *expected {
        return Err(protected_failure());
    }
    let quarantine_name = quarantine_artifact_name(key);
    let destination = directory.join(&quarantine_name);
    if fs::symlink_metadata(&destination).is_ok() {
        return Err(protected_failure());
    }
    let metadata = EnvelopeMetadata::new(EnvelopeKind::Recovery, quarantine_name, expected.len)
        .map_err(protected_error)?;
    let (temp, guard) = create_protected_temp(directory, key)?;
    let mut encrypted =
        create_envelope_writer(temp, password, metadata).map_err(protected_error)?;
    io::copy(&mut source, &mut encrypted).map_err(protected_error)?;
    let mut temp = encrypted.finish().map_err(protected_error)?;
    temp.flush().map_err(protected_error)?;
    temp.sync_all().map_err(protected_error)?;
    drop(temp);
    if ArtifactVersion::from_metadata(&fs::symlink_metadata(&source_path).map_err(protected_error)?)
        != *expected
    {
        return Err(protected_failure());
    }
    fs::hard_link(guard.path(), &destination).map_err(protected_error)?;
    sync_recovery_directory(directory)?;
    if ArtifactVersion::from_metadata(&fs::symlink_metadata(&source_path).map_err(protected_error)?)
        != *expected
    {
        let _ = fs::remove_file(&destination);
        return Err(protected_failure());
    }
    fs::remove_file(&source_path).map_err(protected_error)?;
    sync_recovery_directory(directory)?;
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ArtifactVersion {
    #[cfg(windows)]
    digest: [u8; 32],
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(any(unix, windows))]
    device: u64,
    #[cfg(any(unix, windows))]
    inode: u64,
    #[cfg(any(unix, windows))]
    changed_seconds: i64,
    #[cfg(any(unix, windows))]
    changed_nanoseconds: i64,
}

impl ArtifactVersion {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            #[cfg(windows)]
            digest: metadata.digest(),
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(any(unix, windows))]
            device: metadata.dev(),
            #[cfg(any(unix, windows))]
            inode: metadata.ino(),
            #[cfg(any(unix, windows))]
            changed_seconds: metadata.ctime(),
            #[cfg(any(unix, windows))]
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }

    /// Same file with the same content identity, ignoring the inode change
    /// time that `link(2)` updates on the source it is called for.
    fn same_linked_content(&self, other: &Self) -> bool {
        #[cfg(any(unix, windows))]
        let same_inode = self.device == other.device && self.inode == other.inode;
        #[cfg(not(any(unix, windows)))]
        let same_inode = true;
        #[cfg(windows)]
        if self.digest != other.digest {
            return false;
        }
        self.len == other.len && self.modified == other.modified && same_inode
    }
}

fn protected_artifact_version(path: &Path) -> Result<ArtifactVersion, RecoveryError> {
    let metadata = fs::symlink_metadata(path).map_err(protected_error)?;
    if !metadata.file_type().is_file() {
        return Err(protected_failure());
    }
    Ok(ArtifactVersion::from_metadata(&metadata))
}

fn has_age_prefix(file: &mut File) -> Result<bool, RecoveryError> {
    let mut prefix = [0_u8; AGE_PREFIX.len()];
    let mut read_total = 0;
    while read_total < prefix.len() {
        let read = file.read(&mut prefix[read_total..]).map_err(io_error)?;
        if read == 0 {
            break;
        }
        read_total += read;
    }
    file.seek(SeekFrom::Start(0)).map_err(io_error)?;
    Ok(read_total == prefix.len() && is_age_prefix(&prefix))
}

fn protected_failure() -> RecoveryError {
    RecoveryError::InvalidArtifact("protected recovery operation failed".to_owned())
}

fn protected_error(_error: impl std::fmt::Display) -> RecoveryError {
    protected_failure()
}

fn sync_recovery_directory(directory: &Path) -> Result<(), RecoveryError> {
    stillus_platform::sync_directory(directory).map_err(io_error)
}

fn create_envelope_writer<W: Write>(
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

#[cfg(any(unix, windows))]
fn create_temp(directory: &Path, key: &RecoveryKey) -> Result<(File, TempGuard), RecoveryError> {
    for _ in 0..32 {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let name = format!(".{}.tmp-{}-{id}", key.artifact_name, std::process::id());
        let path = directory.join(name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        match options.open(&path) {
            Ok(file) => return Ok((file, TempGuard::new(path))),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error(error)),
        }
    }
    Err(RecoveryError::Io(
        "could not allocate recovery temp file".to_owned(),
    ))
}

#[cfg(any(unix, windows))]
fn create_protected_temp(
    directory: &Path,
    key: &RecoveryKey,
) -> Result<(File, TempGuard), RecoveryError> {
    for _ in 0..32 {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let name = format!(
            ".protected-{}.tmp-{}-{id}",
            key.artifact_name,
            std::process::id()
        );
        let path = directory.join(name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        match options.open(&path) {
            Ok(file) => return Ok((file, TempGuard::new(path))),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(protected_error(error)),
        }
    }
    Err(protected_failure())
}

#[cfg(any(unix, windows))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProtectedTempName {
    process_id: u32,
}

#[cfg(any(unix, windows))]
fn protected_temp_name(value: &str) -> Option<ProtectedTempName> {
    if !value.is_ascii() {
        return None;
    }
    let value = value.strip_prefix(".protected-")?;
    if value.len() < 32 {
        return None;
    }
    let (artifact_hash, suffix) = value.split_at(32);
    if !artifact_hash
        .bytes()
        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return None;
    }
    let suffix = suffix.strip_prefix(".nrrec.tmp-")?;
    let (process_id, temp_id) = suffix.split_once('-')?;
    if temp_id.contains('-') || !is_canonical_decimal(process_id) || !is_canonical_decimal(temp_id)
    {
        return None;
    }
    let process_id = process_id.parse::<u32>().ok()?;
    if process_id == 0 || temp_id.parse::<u64>().is_err() {
        return None;
    }
    Some(ProtectedTempName { process_id })
}

fn is_canonical_decimal(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && (value == "0" || !value.starts_with('0'))
}

#[cfg(any(unix, windows))]
fn safely_stale(metadata: &fs::Metadata, now: std::time::SystemTime) -> bool {
    metadata
        .modified()
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age >= PROTECTED_TEMP_STALE_AFTER)
}

#[cfg(any(unix, windows))]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
}

#[cfg(any(unix, windows))]
fn read_bounded_age_line(
    reader: &mut impl BufRead,
    consumed: &mut usize,
) -> Result<Option<Vec<u8>>, RecoveryError> {
    let mut line = Vec::new();
    let read = reader.read_until(b'\n', &mut line).map_err(io_error)?;
    *consumed = consumed.saturating_add(read);
    if read == 0 || *consumed > MAX_AGE_HEADER_BYTES || !line.ends_with(b"\n") {
        return Ok(None);
    }
    Ok(Some(line))
}

#[cfg(any(unix, windows))]
fn is_unpadded_base64(value: &[u8], expected_len: Option<usize>) -> bool {
    expected_len.is_none_or(|length| value.len() == length)
        && !value.is_empty()
        && value
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'+' || *byte == b'/')
}

#[cfg(any(unix, windows))]
fn has_structural_age_envelope(file: &mut File) -> Result<bool, RecoveryError> {
    file.seek(SeekFrom::Start(0)).map_err(io_error)?;
    let mut reader = BufReader::new(file);
    let mut consumed = 0_usize;
    let Some(version) = read_bounded_age_line(&mut reader, &mut consumed)? else {
        return Ok(false);
    };
    if version != AGE_PREFIX {
        return Ok(false);
    }

    let Some(recipient) = read_bounded_age_line(&mut reader, &mut consumed)? else {
        return Ok(false);
    };
    let Some(recipient) = recipient.strip_suffix(b"\n") else {
        return Ok(false);
    };
    let Some(arguments) = recipient.strip_prefix(b"-> scrypt ") else {
        return Ok(false);
    };
    let mut arguments = arguments.split(|byte| *byte == b' ');
    let (Some(salt), Some(work_factor), None) =
        (arguments.next(), arguments.next(), arguments.next())
    else {
        return Ok(false);
    };
    let work_factor = std::str::from_utf8(work_factor)
        .ok()
        .filter(|value| is_canonical_decimal(value))
        .and_then(|value| value.parse::<u8>().ok());
    if !is_unpadded_base64(salt, Some(22)) || !matches!(work_factor, Some(1..=64)) {
        return Ok(false);
    }

    // Stillus passphrase envelopes contain one canonical 32-byte scrypt share
    // (43 unpadded base64 characters) followed by the canonical 32-byte MAC.
    let Some(body) = read_bounded_age_line(&mut reader, &mut consumed)? else {
        return Ok(false);
    };
    let Some(body) = body.strip_suffix(b"\n") else {
        return Ok(false);
    };
    if !is_unpadded_base64(body, Some(43)) {
        return Ok(false);
    }
    let Some(mac) = read_bounded_age_line(&mut reader, &mut consumed)? else {
        return Ok(false);
    };
    let Some(mac) = mac
        .strip_suffix(b"\n")
        .and_then(|line| line.strip_prefix(b"--- "))
    else {
        return Ok(false);
    };
    if !is_unpadded_base64(mac, Some(43)) {
        return Ok(false);
    }

    // A binary age payload contains a 16-byte stream nonce followed by at
    // least one 16-byte authentication tag. A header-only or truncated temp is
    // not sufficient evidence that this is an app-owned encrypted artifact.
    let mut payload_proof = [0_u8; 32];
    match reader.read_exact(&mut payload_proof) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(io_error(error)),
    }
}

#[cfg(any(unix, windows))]
fn cleanup_protected_temps(directory: &Path) -> Result<(), RecoveryError> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error(error)),
    };
    let now = std::time::SystemTime::now();
    let mut removed = false;
    for entry in entries {
        let entry = entry.map_err(io_error)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(owned_name) = protected_temp_name(name) else {
            continue;
        };
        // A temp from this process can still be held by an in-flight writer.
        // PID reuse can delay cleanup until the next launch, which is safer
        // than unlinking a live encrypted write.
        if owned_name.process_id == std::process::id() {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path()).map_err(io_error)?;
        if !metadata.file_type().is_file() || metadata.nlink() != 1 || !safely_stale(&metadata, now)
        {
            continue;
        }
        let mut file = File::open(entry.path()).map_err(io_error)?;
        let opened = file.metadata().map_err(io_error)?;
        if opened.nlink() != 1
            || !same_file(&metadata, &opened)
            || !has_structural_age_envelope(&mut file)?
        {
            continue;
        }
        let current = fs::symlink_metadata(entry.path()).map_err(io_error)?;
        if !current.file_type().is_file()
            || current.nlink() != 1
            || !same_file(&opened, &current)
            || !safely_stale(&current, now)
        {
            continue;
        }
        fs::remove_file(entry.path()).map_err(io_error)?;
        removed = true;
    }
    if removed {
        sync_recovery_directory(directory)?;
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn cleanup_protected_temps(_directory: &Path) -> Result<(), RecoveryError> {
    Ok(())
}

struct TempGuard {
    path: PathBuf,
    armed: bool,
}

impl TempGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

struct HashingWriter<'a> {
    writer: &'a mut dyn Write,
    written: u64,
    hash: u64,
}

impl<'a> HashingWriter<'a> {
    fn new(writer: &'a mut dyn Write) -> Self {
        Self {
            writer,
            written: 0,
            hash: FNV_OFFSET_1,
        }
    }
}

impl Write for HashingWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.writer.write(buffer)?;
        self.written = self.written.saturating_add(written as u64);
        self.hash = fnv(self.hash, &buffer[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

fn fnv(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn io_error(error: impl std::fmt::Display) -> RecoveryError {
    RecoveryError::Io(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn store() -> (PathBuf, RecoveryStore, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "stillus-recovery-test-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("notes")).unwrap();
        let note = root.join("notes").join("Пример.md");
        fs::write(&note, b"canonical").unwrap();
        (root.clone(), RecoveryStore::new(&root), note)
    }

    #[test]
    fn new_recovery_round_trips_without_loading_or_modifying_legacy_work() {
        let (root, store, note) = store();
        let key = store.key_for_note(&note).unwrap();
        let legacy_directory = root.join(".legacy/recovery");
        fs::create_dir_all(&legacy_directory).unwrap();
        let mut legacy = Vec::new();
        write_header(&mut legacy, &key, 1, 123, 0, FNV_OFFSET_1).unwrap();
        legacy.splice(..MAGIC.len(), b"LEGACYRECOVERY1\n".iter().copied());
        let legacy_path = legacy_directory.join(&key.artifact_name);
        fs::write(&legacy_path, &legacy).unwrap();
        assert!(read_header(&mut legacy.as_slice()).is_err());
        assert!(store.scan().records.is_empty());
        assert!(!root.join(".stillus").exists());

        let body = b"new unsaved work";
        store
            .write(
                &key,
                2,
                123,
                body.len() as u64,
                fnv(FNV_OFFSET_1, body),
                |writer| writer.write_all(body),
            )
            .unwrap();
        let current_path = root.join(".stillus/recovery").join(&key.artifact_name);
        assert!(
            fs::read(current_path)
                .unwrap()
                .starts_with(b"STILLUSRECOVERY1\n")
        );
        let mut opened = store.open(&key).unwrap();
        let mut recovered = Vec::new();
        opened.body.read_to_end(&mut recovered).unwrap();
        assert_eq!(recovered, body);
        assert_eq!(opened.record.revision, 2);
        assert_eq!(fs::read(legacy_path).unwrap(), legacy);
        assert_eq!(fs::read(note).unwrap(), b"canonical");
        drop(opened);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn another_window_cannot_overwrite_or_autoclean_unsaved_work() {
        let root =
            std::env::temp_dir().join(format!("stillus-recovery-windows-{}", std::process::id()));
        fs::create_dir_all(root.join("notes")).unwrap();
        let first = RecoveryStore::new(&root);
        let second = RecoveryStore::new(&root);
        let key = first.key_for_note(root.join("notes/note.md")).unwrap();
        first
            .write(&key, 1, 123, 0, FNV_OFFSET_1, |_| Ok(()))
            .unwrap();
        assert!(
            second
                .write(&key, 999, 456, 0, FNV_OFFSET_1, |_| Ok(()))
                .is_err()
        );
        assert!(!second.remove_saved(&key, 999).unwrap());
        assert!(second.remove(&key).is_err());
        assert_eq!(first.open(&key).unwrap().record.base_checksum, 123);
        assert!(first.remove_saved(&key, 1).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn external_recovery_keys_are_namespaced_and_stable() {
        let (root, store, _note) = store();
        let first = store
            .key_for_external("markdown", "external/0123456789abcdef")
            .unwrap();
        let second = store
            .key_for_external("markdown", "external/0123456789abcdef")
            .unwrap();
        let other = store
            .key_for_external("sheets", "external/0123456789abcdef")
            .unwrap();
        assert_eq!(first, second);
        assert_ne!(first, other);
        assert!(first.relative_path().starts_with("external/markdown/"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn writes_discovers_streams_and_revision_gates_cleanup() {
        let (root, store, note) = store();
        let key = store.key_for_note(&note).unwrap();
        let body = "local 🦀\n";
        let checksum = fnv(FNV_OFFSET_1, body.as_bytes());
        store
            .write(&key, 2, 123, body.len() as u64, checksum, |writer| {
                writer.write_all(body.as_bytes())
            })
            .unwrap();
        let scan = store.scan();
        assert!(scan.diagnostics.is_empty());
        assert_eq!(scan.records.len(), 1);
        assert_eq!(scan.records[0].revision, 2);
        let mut artifact = store.open(&key).unwrap();
        let mut output = String::new();
        artifact.body.read_to_string(&mut output).unwrap();
        assert_eq!(output, body);
        assert!(!store.remove_saved(&key, 1).unwrap());
        assert!(store.remove_saved(&key, 2).unwrap());
        assert!(store.scan().records.is_empty());
        assert_eq!(fs::read(note).unwrap(), b"canonical");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_or_malformed_artifact_never_touches_canonical_note() {
        let (root, store, note) = store();
        let key = store.key_for_note(&note).unwrap();
        let error = store
            .write(&key, 1, 0, 4, 0, |writer| writer.write_all(b"bad"))
            .unwrap_err();
        assert!(matches!(error, RecoveryError::InvalidArtifact(_)));
        assert!(store.scan().records.is_empty());
        assert_eq!(fs::read(&note).unwrap(), b"canonical");

        let directory = store.ensure_directory().unwrap();
        fs::write(directory.join("broken.nrrec"), b"broken").unwrap();
        let unusable = b"sole potentially recoverable bytes";
        fs::write(directory.join(&key.artifact_name), unusable).unwrap();
        store
            .write(&key, 2, 0, 0, FNV_OFFSET_1, |_writer| Ok(()))
            .unwrap();
        let scan = store.scan();
        assert_eq!(scan.records.len(), 1);
        assert_eq!(scan.diagnostics.len(), 2);
        let quarantine = fs::read_dir(&directory)
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| is_quarantine_artifact_name(&entry.file_name()))
            .unwrap();
        assert_eq!(fs::read(quarantine.path()).unwrap(), unusable);
        assert!(store.remove_saved(&key, 2).unwrap());
        assert_eq!(fs::read(note).unwrap(), b"canonical");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn linked_content_identity_ignores_change_time_only() {
        let (root, store, note) = store();
        let key = store.key_for_note(&note).unwrap();
        let directory = store.ensure_directory().unwrap();
        let artifact = directory.join(&key.artifact_name);
        fs::write(&artifact, b"unusable").unwrap();
        let expected = ArtifactVersion::from_metadata(&fs::symlink_metadata(&artifact).unwrap());

        let mut linked = expected.clone();
        linked.changed_seconds += 1;
        linked.changed_nanoseconds = 0;
        assert_ne!(linked, expected);
        assert!(linked.same_linked_content(&expected));

        let mut replaced = expected.clone();
        replaced.inode = replaced.inode.wrapping_add(1);
        assert!(!replaced.same_linked_content(&expected));
        let mut rewritten = expected.clone();
        rewritten.len += 1;
        assert!(!rewritten.same_linked_content(&expected));
        let mut touched = expected.clone();
        touched.modified = expected
            .modified
            .map(|modified| modified + std::time::Duration::from_secs(1));
        assert!(!touched.same_linked_content(&expected));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn valid_other_key_collision_is_preserved_and_never_quarantined() {
        let (root, store, note) = store();
        let key = store.key_for_note(note).unwrap();
        let directory = store.ensure_directory().unwrap();
        let collision = RecoveryKey {
            relative_path: "notes/hash-collision.md".to_owned(),
            artifact_name: key.artifact_name.clone(),
        };
        let path = directory.join(&key.artifact_name);
        let mut file = File::create(&path).unwrap();
        write_header(&mut file, &collision, 7, 1, 0, FNV_OFFSET_1).unwrap();
        file.sync_all().unwrap();
        drop(file);
        let original = fs::read(&path).unwrap();

        let error = store
            .write(&key, 8, 2, 0, FNV_OFFSET_1, |_writer| Ok(()))
            .unwrap_err();
        assert!(matches!(
            error,
            RecoveryError::InvalidArtifact(ref message) if message.contains("hash collision")
        ));
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(
            fs::read_dir(&directory)
                .unwrap()
                .map(Result::unwrap)
                .all(|entry| !is_quarantine_artifact_name(&entry.file_name()))
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn artifact_symlink_is_rejected_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let (root, store, note) = store();
        let key = store.key_for_note(note).unwrap();
        let directory = store.ensure_directory().unwrap();
        let target = root.join("foreign-recovery-target");
        let target_bytes = b"foreign hard boundary";
        fs::write(&target, target_bytes).unwrap();
        let artifact = directory.join(&key.artifact_name);
        symlink(&target, &artifact).unwrap();

        assert!(matches!(
            store.write(&key, 1, 0, 0, FNV_OFFSET_1, |_writer| Ok(())),
            Err(RecoveryError::InvalidArtifact(_))
        ));
        assert!(
            artifact
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(target).unwrap(), target_bytes);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_store_symlink() {
        use std::os::unix::fs::symlink;

        let (root, store, note) = store();
        let outside = root.join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, root.join(".stillus")).unwrap();
        let key = store.key_for_note(note).unwrap();
        assert!(matches!(
            store.open(&key),
            Err(RecoveryError::InvalidStore(_))
        ));
        assert!(matches!(
            store.protected_exists(&key),
            Err(RecoveryError::InvalidStore(_))
        ));
        assert!(matches!(
            store.remove(&key),
            Err(RecoveryError::InvalidStore(_))
        ));
        assert_eq!(store.scan().diagnostics.len(), 1);
        assert_eq!(store.scan_protected().diagnostics.len(), 1);
        let error = store
            .write(&key, 1, 0, 0, FNV_OFFSET_1, |_writer| Ok(()))
            .unwrap_err();
        assert!(matches!(error, RecoveryError::InvalidStore(_)));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn public_operations_reject_recovery_directory_symlink_without_touching_external_artifact() {
        use std::os::unix::fs::symlink;

        let (root, store, note) = store();
        let key = store.key_for_note(note).unwrap();
        let hidden = root.join(".stillus");
        let outside = root.join("outside-recovery");
        fs::create_dir(&hidden).unwrap();
        fs::create_dir(&outside).unwrap();
        let external_artifact = outside.join(&key.artifact_name);
        let external_bytes = b"external deterministic artifact must survive byte-for-byte";
        fs::write(&external_artifact, external_bytes).unwrap();
        symlink(&outside, hidden.join("recovery")).unwrap();
        let password = MasterPassword::new("symlink guard password".to_owned());

        assert!(matches!(
            store.open(&key),
            Err(RecoveryError::InvalidStore(_))
        ));
        assert!(matches!(
            store.open_protected(&key, &password),
            Err(RecoveryError::InvalidStore(_))
        ));
        assert!(matches!(
            store.protected_exists(&key),
            Err(RecoveryError::InvalidStore(_))
        ));
        assert!(matches!(
            store.remove_saved(&key, u64::MAX),
            Err(RecoveryError::InvalidStore(_))
        ));
        assert!(matches!(
            store.remove(&key),
            Err(RecoveryError::InvalidStore(_))
        ));
        assert!(matches!(
            store.remove_protected(&key),
            Err(RecoveryError::InvalidStore(_))
        ));
        assert!(matches!(
            store.remove_protected_saved(&key, &password, u64::MAX),
            Err(RecoveryError::InvalidStore(_))
        ));
        assert!(matches!(
            store.write(&key, 1, 0, 0, FNV_OFFSET_1, |_writer| Ok(())),
            Err(RecoveryError::InvalidStore(_))
        ));
        assert!(matches!(
            store.write_protected(&key, &password, 1, 0, 0, FNV_OFFSET_1, |_writer| Ok(())),
            Err(RecoveryError::InvalidStore(_))
        ));
        assert!(store.scan().records.is_empty());
        assert_eq!(store.scan().diagnostics.len(), 1);
        assert!(store.scan_protected().records.is_empty());
        assert_eq!(store.scan_protected().diagnostics.len(), 1);
        assert_eq!(fs::read(&external_artifact).unwrap(), external_bytes);

        fs::remove_file(hidden.join("recovery")).unwrap();
        fs::write(hidden.join("recovery"), b"not a directory").unwrap();
        assert!(matches!(
            store.protected_exists(&key),
            Err(RecoveryError::InvalidStore(_))
        ));
        assert!(matches!(
            store.remove(&key),
            Err(RecoveryError::InvalidStore(_))
        ));
        assert_eq!(fs::read(&external_artifact).unwrap(), external_bytes);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn protected_recovery_is_opaque_authenticated_and_strictly_removed() {
        let (root, store, note) = store();
        let key = store.key_for_note(&note).unwrap();
        let password = MasterPassword::new("correct recovery password".to_owned());
        let wrong_password = MasterPassword::new("wrong recovery password".to_owned());
        let body = b"protected-recovery-body-marker-7f31";
        let checksum = fnv(FNV_OFFSET_1, body);
        let directory = store.ensure_directory().unwrap();
        let artifact_path = directory.join(&key.artifact_name);

        let injected = store
            .write_protected(
                &key,
                &password,
                3,
                0x1122_3344_5566_7788,
                body.len() as u64,
                checksum,
                |writer| {
                    writer.write_all(body)?;
                    Err(io::Error::other("injected protected recovery failure"))
                },
            )
            .unwrap_err();
        assert!(matches!(injected, RecoveryError::Io(_)));
        assert!(!artifact_path.exists());
        assert!(fs::read_dir(&directory).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".protected-")
        }));

        let expected = store
            .write_protected(
                &key,
                &password,
                4,
                0x1122_3344_5566_7788,
                body.len() as u64,
                checksum,
                |writer| writer.write_all(body),
            )
            .unwrap();
        let ciphertext = fs::read(&artifact_path).unwrap();
        assert!(is_age_prefix(&ciphertext));
        for marker in [MAGIC, key.relative_path.as_bytes(), body.as_slice()] {
            assert!(
                !ciphertext
                    .windows(marker.len())
                    .any(|candidate| candidate == marker),
                "protected recovery leaked marker"
            );
        }
        assert!(store.scan().records.is_empty());
        assert!(store.scan().diagnostics.is_empty());
        assert!(store.protected_exists(&key).unwrap());
        let protected_scan = store.scan_protected();
        assert!(protected_scan.diagnostics.is_empty());
        assert_eq!(
            protected_scan.records,
            vec![ProtectedRecoveryRecord {
                artifact_name: key.artifact_name.clone(),
            }]
        );

        let before_wrong_password = fs::read(&artifact_path).unwrap();
        let wrong = match store.open_protected(&key, &wrong_password) {
            Ok(_) => panic!("wrong password unexpectedly opened protected recovery"),
            Err(error) => error,
        };
        assert_eq!(wrong, protected_failure());
        assert_eq!(fs::read(&artifact_path).unwrap(), before_wrong_password);

        let mut artifact = store.open_protected(&key, &password).unwrap();
        assert_eq!(artifact.record, expected);
        let mut recovered = Vec::new();
        artifact.body.read_to_end(&mut recovered).unwrap();
        assert_eq!(recovered, body);

        let mut tampered = before_wrong_password;
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        fs::write(&artifact_path, &tampered).unwrap();
        let tamper_error = match store.open_protected(&key, &password) {
            Ok(_) => panic!("tampered protected recovery unexpectedly opened"),
            Err(error) => error,
        };
        assert_eq!(tamper_error, protected_failure());
        assert_eq!(fs::read(&artifact_path).unwrap(), tampered);
        fs::write(&artifact_path, ciphertext).unwrap();

        let malformed_temp = directory.join(format!(".protected-{}.tmp-stale", key.artifact_name));
        fs::write(&malformed_temp, b"non-sensitive malformed temp").unwrap();
        assert!(malformed_temp.exists());
        let scan_after_cleanup = store.scan_protected();
        assert!(scan_after_cleanup.diagnostics.is_empty());
        assert!(malformed_temp.exists());

        assert!(!store.remove_protected_saved(&key, &password, 3).unwrap());
        assert!(store.remove_protected_saved(&key, &password, 4).unwrap());
        assert!(!store.protected_exists(&key).unwrap());
        assert!(!store.remove_protected(&key).unwrap());

        std::os::unix::fs::symlink(&note, &artifact_path).unwrap();
        assert!(matches!(
            store.remove_protected(&key),
            Err(RecoveryError::InvalidArtifact(_))
        ));
        fs::remove_file(&artifact_path).unwrap();
        assert_eq!(fs::read(note).unwrap(), b"canonical");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn protected_write_encrypts_unusable_plain_artifact_before_quarantine() {
        let (root, store, note) = store();
        let key = store.key_for_note(note).unwrap();
        let password = MasterPassword::new("quarantine encryption password".to_owned());
        let directory = store.ensure_directory().unwrap();
        let artifact = directory.join(&key.artifact_name);
        let stale_plaintext = b"stale plaintext recovery marker";
        fs::write(&artifact, stale_plaintext).unwrap();

        store
            .write_protected(&key, &password, 2, 5, 0, FNV_OFFSET_1, |_writer| Ok(()))
            .unwrap();

        let final_bytes = fs::read(&artifact).unwrap();
        assert!(is_age_prefix(&final_bytes));
        assert!(
            !final_bytes
                .windows(stale_plaintext.len())
                .any(|window| window == stale_plaintext)
        );
        let quarantine = fs::read_dir(&directory)
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| is_quarantine_artifact_name(&entry.file_name()))
            .unwrap();
        let quarantine_bytes = fs::read(quarantine.path()).unwrap();
        assert!(is_age_prefix(&quarantine_bytes));
        assert!(
            !quarantine_bytes
                .windows(stale_plaintext.len())
                .any(|window| window == stale_plaintext)
        );
        let mut decrypted = decrypt(
            File::open(quarantine.path()).unwrap(),
            &password,
            EnvelopeKind::Recovery,
        )
        .unwrap();
        let mut preserved = Vec::new();
        decrypted.read_to_end(&mut preserved).unwrap();
        assert_eq!(preserved, stale_plaintext);
        assert_eq!(store.scan().diagnostics.len(), 1);
        assert_eq!(store.scan_protected().records.len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn protected_temp_cleanup_requires_owned_stale_complete_unlinked_envelope() {
        use std::fs::FileTimes;

        fn temp_path(directory: &Path, key: &RecoveryKey, process_id: u32, id: u64) -> PathBuf {
            directory.join(format!(
                ".protected-{}.tmp-{process_id}-{id}",
                key.artifact_name
            ))
        }

        fn write_envelope(path: &Path, key: &RecoveryKey, password: &MasterPassword) {
            let output = File::create(path).unwrap();
            let metadata =
                EnvelopeMetadata::new(EnvelopeKind::Recovery, key.artifact_name.clone(), 0)
                    .unwrap();
            let mut output = create_envelope_writer(output, password, metadata)
                .unwrap()
                .finish()
                .unwrap();
            output.flush().unwrap();
            output.sync_all().unwrap();
        }

        fn stale_times() -> FileTimes {
            let stale_at = std::time::SystemTime::now()
                .checked_sub(PROTECTED_TEMP_STALE_AFTER + std::time::Duration::from_secs(60))
                .unwrap();
            FileTimes::new().set_modified(stale_at)
        }

        fn make_stale(path: &Path) {
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(stale_times())
                .unwrap();
        }

        let (root, store, note) = store();
        let key = store.key_for_note(note).unwrap();
        let password = MasterPassword::new("temp cleanup password".to_owned());
        let directory = store.ensure_directory().unwrap();
        let inactive_pid = if std::process::id() == u32::MAX {
            std::process::id() - 1
        } else {
            std::process::id() + 1
        };

        let removable = temp_path(&directory, &key, inactive_pid, 1);
        write_envelope(&removable, &key, &password);
        make_stale(&removable);

        let fresh = temp_path(&directory, &key, inactive_pid, 2);
        write_envelope(&fresh, &key, &password);

        let malformed = temp_path(&directory, &key, inactive_pid, 3);
        fs::write(&malformed, AGE_PREFIX).unwrap();
        make_stale(&malformed);

        let hardlinked = temp_path(&directory, &key, inactive_pid, 4);
        write_envelope(&hardlinked, &key, &password);
        make_stale(&hardlinked);
        let hardlink_alias = directory.join("foreign-hardlink-alias");
        fs::hard_link(&hardlinked, &hardlink_alias).unwrap();

        let foreign = directory.join(format!(
            ".protected-{}.tmp-{inactive_pid}-5-extra",
            key.artifact_name
        ));
        write_envelope(&foreign, &key, &password);
        make_stale(&foreign);

        let (active_file, active_guard) = create_protected_temp(&directory, &key).unwrap();
        let active = active_guard.path().to_path_buf();
        let metadata =
            EnvelopeMetadata::new(EnvelopeKind::Recovery, key.artifact_name.clone(), 0).unwrap();
        let mut active_file = create_envelope_writer(active_file, &password, metadata)
            .unwrap()
            .finish()
            .unwrap();
        active_file.flush().unwrap();
        active_file.sync_all().unwrap();
        active_file.set_times(stale_times()).unwrap();

        let scan = store.scan_protected();
        assert!(scan.diagnostics.is_empty());
        assert!(!removable.exists());
        assert!(fresh.exists());
        assert!(malformed.exists());
        assert!(hardlinked.exists());
        assert!(hardlink_alias.exists());
        assert!(foreign.exists());
        assert!(active.exists());

        drop(active_file);
        drop(active_guard);
        assert!(!active.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
