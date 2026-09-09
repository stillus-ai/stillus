// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! Canonical workspace verifier and engine-secret storage.

use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use stillus_platform::fs::{self, File, OpenOptions};

use rand::RngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use stillus_engine::{
    EngineError, EngineId, ItemId, ReferencedSecret, SecretRef, SecretResolver, SecretValue,
};
use stillus_secure::{
    ARMORED_AGE_CRLF_PREFIX, ArmoredEnvelopeWriter, EnvelopeKind, EnvelopeMetadata, MasterPassword,
    decrypt_armored,
};
use zeroize::{Zeroize, Zeroizing};

const STORE_NAME: &str = ".stillus_security";
const VERIFIER_NAME: &str = "master.age";
const SECRETS_NAME: &str = "secrets";
const PAYLOAD_VERSION: u32 = 1;
const VERIFIER_RANDOM_BYTES: usize = 64;
const MAX_SECURITY_PAYLOAD: usize = 128 * 1024;
const MAX_SECURITY_FILE_BYTES: u64 = 512 * 1024;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceSecurityState {
    Unconfigured,
    ConfiguredLocked,
    Unlocked,
    LegacyLocked,
    Blocked,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VaultId(String);

impl VaultId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecretBinding {
    pub engine_id: EngineId,
    pub owner: ItemId,
    pub field_key: String,
}

impl SecretBinding {
    pub fn new(
        engine_id: EngineId,
        owner: ItemId,
        field_key: impl Into<String>,
    ) -> Result<Self, SecurityError> {
        let field_key = field_key.into();
        validate_key(&field_key)?;
        Ok(Self {
            engine_id,
            owner,
            field_key,
        })
    }
}

#[derive(Clone, Debug)]
pub struct SecurityCatalog {
    pub state: WorkspaceSecurityState,
    pub verifier: Option<PathBuf>,
    pub secrets: Vec<PathBuf>,
}

#[derive(Clone)]
pub struct SecurityStore {
    workspace: PathBuf,
}

impl SecurityStore {
    pub fn new(workspace: impl AsRef<Path>) -> Self {
        Self {
            workspace: workspace.as_ref().to_path_buf(),
        }
    }

    pub fn root(&self) -> PathBuf {
        self.workspace.join(STORE_NAME)
    }

    pub fn verifier_path(&self) -> PathBuf {
        self.root().join(VERIFIER_NAME)
    }

    pub fn secret_path(&self, reference: &SecretRef) -> PathBuf {
        self.root()
            .join(SECRETS_NAME)
            .join(format!("{}.age", reference.as_str()))
    }

    pub fn inspect(&self, legacy_protected_data: bool) -> Result<SecurityCatalog, SecurityError> {
        let root = self.root();
        let root_metadata = match fs::symlink_metadata(&root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(SecurityCatalog {
                    state: if legacy_protected_data {
                        WorkspaceSecurityState::LegacyLocked
                    } else {
                        WorkspaceSecurityState::Unconfigured
                    },
                    verifier: None,
                    secrets: Vec::new(),
                });
            }
            Err(error) => return Err(SecurityError::Io(error.to_string())),
        };
        validate_directory(&root, &root_metadata)?;
        let secrets_directory = root.join(SECRETS_NAME);
        let secrets = inspect_secrets_directory(&secrets_directory)?;
        for entry in fs::read_dir(&root).map_err(security_io)? {
            let entry = entry.map_err(security_io)?;
            if entry.file_name() != VERIFIER_NAME && entry.file_name() != SECRETS_NAME {
                return Err(SecurityError::Blocked(format!(
                    "unknown workspace security entry {}",
                    entry.path().display()
                )));
            }
        }
        let verifier = self.verifier_path();
        match fs::symlink_metadata(&verifier) {
            Ok(metadata) => {
                validate_file(&verifier, &metadata)?;
                validate_armored_prefix(&verifier)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if !secrets.is_empty() {
                    return Err(SecurityError::Blocked(
                        "workspace secrets exist without a verifier".to_owned(),
                    ));
                }
                return Ok(SecurityCatalog {
                    state: if legacy_protected_data {
                        WorkspaceSecurityState::LegacyLocked
                    } else {
                        WorkspaceSecurityState::Unconfigured
                    },
                    verifier: None,
                    secrets,
                });
            }
            Err(error) => return Err(SecurityError::Io(error.to_string())),
        }
        Ok(SecurityCatalog {
            state: WorkspaceSecurityState::ConfiguredLocked,
            verifier: Some(verifier),
            secrets,
        })
    }

    pub fn configure(&self, password: &MasterPassword) -> Result<VaultId, SecurityError> {
        let _operation = stillus_platform::OperationLock::directory(&self.workspace)
            .map_err(|error| SecurityError::Io(error.to_string()))?;
        if password.is_empty() {
            return Err(SecurityError::Invalid(
                "master password is empty".to_owned(),
            ));
        }
        let existing = self.inspect(false)?;
        if existing.verifier.is_some() {
            return self.unlock(password);
        }
        let root = self.root();
        ensure_private_directory(&root)?;
        ensure_private_directory(&root.join(SECRETS_NAME))?;
        let vault_id = VaultId(random_hex(16));
        let mut random = vec![0_u8; VERIFIER_RANDOM_BYTES];
        OsRng.fill_bytes(&mut random);
        let payload = serde_json::to_vec(&VerifierPayload {
            version: PAYLOAD_VERSION,
            vault_id: vault_id.clone(),
            random,
        })
        .map_err(|error| SecurityError::Invalid(error.to_string()))?;
        write_armored_file(
            &root,
            &self.verifier_path(),
            password,
            EnvelopeKind::WorkspaceVerifier,
            VERIFIER_NAME,
            &payload,
        )?;
        let verified = self.unlock(password)?;
        if verified != vault_id {
            return Err(SecurityError::Blocked(
                "new workspace verifier changed during creation".to_owned(),
            ));
        }
        Ok(vault_id)
    }

    pub fn unlock(&self, password: &MasterPassword) -> Result<VaultId, SecurityError> {
        let path = self.verifier_path();
        let metadata = fs::symlink_metadata(&path).map_err(security_io)?;
        validate_file(&path, &metadata)?;
        validate_armored_prefix(&path)?;
        let payload = read_armored_payload(&path, password, EnvelopeKind::WorkspaceVerifier)?;
        let verifier: VerifierPayload =
            serde_json::from_slice(&payload).map_err(|_| SecurityError::AuthenticationFailed)?;
        if verifier.version != PAYLOAD_VERSION
            || verifier.random.len() < VERIFIER_RANDOM_BYTES
            || !is_hex_id(verifier.vault_id.as_str(), 32)
        {
            return Err(SecurityError::Blocked(
                "workspace verifier payload is invalid".to_owned(),
            ));
        }
        Ok(verifier.vault_id)
    }

    pub fn store_secret(
        &self,
        binding: &SecretBinding,
        value: &SecretValue,
        password: &MasterPassword,
    ) -> Result<SecretRef, SecurityError> {
        let _operation = stillus_platform::OperationLock::directory(&self.workspace)
            .map_err(|error| SecurityError::Io(error.to_string()))?;
        let vault_id = match self.inspect(false)?.state {
            WorkspaceSecurityState::Unconfigured => self.configure(password)?,
            WorkspaceSecurityState::ConfiguredLocked => self.unlock(password)?,
            WorkspaceSecurityState::LegacyLocked => {
                return Err(SecurityError::Blocked(
                    "legacy workspace must be unlocked before storing secrets".to_owned(),
                ));
            }
            WorkspaceSecurityState::Blocked => {
                return Err(SecurityError::Blocked(
                    "workspace security is blocked".to_owned(),
                ));
            }
            WorkspaceSecurityState::Unlocked => self.unlock(password)?,
        };
        let reference = loop {
            let candidate = SecretRef::new(random_hex(16))
                .map_err(|error| SecurityError::Invalid(error.to_string()))?;
            if !self.secret_path(&candidate).exists() {
                break candidate;
            }
        };
        let payload = Zeroizing::new(
            serde_json::to_vec(&SecretPayloadRef {
                version: PAYLOAD_VERSION,
                vault_id: &vault_id,
                engine_id: &binding.engine_id,
                owner: &binding.owner,
                field_key: &binding.field_key,
                value: value.expose(),
            })
            .map_err(|error| SecurityError::Invalid(error.to_string()))?,
        );
        let directory = self.root().join(SECRETS_NAME);
        ensure_private_directory(&directory)?;
        write_armored_file(
            &directory,
            &self.secret_path(&reference),
            password,
            EnvelopeKind::EngineSecret,
            &format!("{}.age", reference.as_str()),
            payload.as_slice(),
        )?;
        let resolved = self.resolve_secret(&reference, binding, password)?;
        if resolved.expose() != value.expose() {
            return Err(SecurityError::Blocked(
                "new secret failed authenticated verification".to_owned(),
            ));
        }
        Ok(reference)
    }

    pub fn resolve_secret(
        &self,
        reference: &SecretRef,
        binding: &SecretBinding,
        password: &MasterPassword,
    ) -> Result<SecretValue, SecurityError> {
        let vault_id = self.unlock(password)?;
        let path = self.secret_path(reference);
        let metadata = fs::symlink_metadata(&path).map_err(security_io)?;
        validate_file(&path, &metadata)?;
        validate_armored_prefix(&path)?;
        let encoded = Zeroizing::new(read_armored_payload(
            &path,
            password,
            EnvelopeKind::EngineSecret,
        )?);
        let mut payload: SecretPayload =
            serde_json::from_slice(&encoded).map_err(|_| SecurityError::AuthenticationFailed)?;
        if payload.version != PAYLOAD_VERSION
            || payload.vault_id != vault_id
            || payload.engine_id != binding.engine_id
            || payload.owner != binding.owner
            || payload.field_key != binding.field_key
        {
            return Err(SecurityError::AuthenticationFailed);
        }
        SecretValue::new(std::mem::take(&mut payload.value))
            .map_err(|error| SecurityError::Invalid(error.to_string()))
    }

    pub fn referenced_secret_paths(
        &self,
        referenced: &[ReferencedSecret],
        password: &MasterPassword,
    ) -> Result<Vec<PathBuf>, SecurityError> {
        let mut paths = Vec::with_capacity(referenced.len());
        for secret in referenced {
            let binding = SecretBinding::new(
                secret.engine_id.clone(),
                secret.owner.clone(),
                secret.field_key.clone(),
            )?;
            let _ = self.resolve_secret(&secret.reference, &binding, password)?;
            paths.push(self.secret_path(&secret.reference));
        }
        paths.sort();
        paths.dedup();
        if paths.len() != referenced.len() {
            return Err(SecurityError::Blocked(
                "multiple settings reference the same secret blob".to_owned(),
            ));
        }
        Ok(paths)
    }

    pub fn resolver<'a>(&'a self, password: &'a MasterPassword) -> WorkspaceSecretResolver<'a> {
        WorkspaceSecretResolver {
            store: self,
            password,
        }
    }
}

pub struct WorkspaceSecretResolver<'a> {
    store: &'a SecurityStore,
    password: &'a MasterPassword,
}

impl SecretResolver for WorkspaceSecretResolver<'_> {
    fn resolve(&self, secret: &ReferencedSecret) -> Result<SecretValue, EngineError> {
        let binding = SecretBinding::new(
            secret.engine_id.clone(),
            secret.owner.clone(),
            secret.field_key.clone(),
        )
        .map_err(|error| EngineError::Io(error.to_string()))?;
        self.store
            .resolve_secret(&secret.reference, &binding, self.password)
            .map_err(|error| match error {
                SecurityError::AuthenticationFailed => EngineError::NeedsUnlock,
                other => EngineError::Io(other.to_string()),
            })
    }
}

#[derive(Serialize, Deserialize)]
struct VerifierPayload {
    version: u32,
    vault_id: VaultId,
    random: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct SecretPayload {
    version: u32,
    vault_id: VaultId,
    engine_id: EngineId,
    owner: ItemId,
    field_key: String,
    value: Vec<u8>,
}

impl Drop for SecretPayload {
    fn drop(&mut self) {
        self.value.zeroize();
    }
}

#[derive(Serialize)]
struct SecretPayloadRef<'a> {
    version: u32,
    vault_id: &'a VaultId,
    engine_id: &'a EngineId,
    owner: &'a ItemId,
    field_key: &'a str,
    value: &'a [u8],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SecurityError {
    Invalid(String),
    AuthenticationFailed,
    Conflict(String),
    Blocked(String),
    Io(String),
}

impl SecurityError {
    pub fn blocks_workspace(&self) -> bool {
        matches!(self, Self::Blocked(_))
    }
}

impl fmt::Display for SecurityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => write!(formatter, "workspace security rejected: {message}"),
            Self::AuthenticationFailed => {
                formatter.write_str("workspace security authentication failed")
            }
            Self::Conflict(message) => write!(formatter, "workspace security conflict: {message}"),
            Self::Blocked(message) => write!(formatter, "workspace security is blocked: {message}"),
            Self::Io(message) => write!(formatter, "workspace security I/O failed: {message}"),
        }
    }
}

impl std::error::Error for SecurityError {}

fn inspect_secrets_directory(directory: &Path) -> Result<Vec<PathBuf>, SecurityError> {
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(SecurityError::Io(error.to_string())),
    };
    validate_directory(directory, &metadata)?;
    let mut paths = Vec::new();
    for entry in fs::read_dir(directory).map_err(security_io)? {
        let entry = entry.map_err(security_io)?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(security_io)?;
        validate_file(&path, &metadata)?;
        validate_armored_prefix(&path)?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| SecurityError::Blocked("secret filename is not UTF-8".to_owned()))?;
        let reference = name
            .strip_suffix(".age")
            .ok_or_else(|| SecurityError::Blocked(format!("unknown secret file {name}")))?;
        SecretRef::new(reference)
            .map_err(|_| SecurityError::Blocked(format!("invalid secret filename {name}")))?;
        paths.push(path);
    }
    paths.sort();
    Ok(paths)
}

#[cfg(any(unix, windows))]
fn validate_directory(path: &Path, metadata: &fs::Metadata) -> Result<(), SecurityError> {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    if !metadata.file_type().is_dir() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(SecurityError::Blocked(format!(
            "{} is not a private directory",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn validate_directory(path: &Path, metadata: &fs::Metadata) -> Result<(), SecurityError> {
    if !metadata.file_type().is_dir() {
        return Err(SecurityError::Blocked(format!(
            "{} is not a directory",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(any(unix, windows))]
fn validate_file(path: &Path, metadata: &fs::Metadata) -> Result<(), SecurityError> {
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    if !metadata.file_type().is_file()
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.len() > MAX_SECURITY_FILE_BYTES
    {
        return Err(SecurityError::Blocked(format!(
            "{} is not a private unlinked file",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn validate_file(path: &Path, metadata: &fs::Metadata) -> Result<(), SecurityError> {
    if !metadata.file_type().is_file() || metadata.len() > MAX_SECURITY_FILE_BYTES {
        return Err(SecurityError::Blocked(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    Ok(())
}

fn validate_armored_prefix(path: &Path) -> Result<(), SecurityError> {
    let mut prefix = vec![0_u8; ARMORED_AGE_CRLF_PREFIX.len()];
    let mut file = File::open(path).map_err(security_io)?;
    file.read_exact(&mut prefix).map_err(|_| {
        SecurityError::Blocked(format!("{} is not an armored age file", path.display()))
    })?;
    if !stillus_secure::is_armored_age_prefix(&prefix) {
        return Err(SecurityError::Blocked(format!(
            "{} is not an armored age file",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(any(unix, windows))]
fn ensure_private_directory(path: &Path) -> Result<(), SecurityError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_directory(path, &metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            stillus_platform::create_private_directory(path).map_err(security_io)?;
            sync_directory(path.parent().ok_or_else(|| {
                SecurityError::Invalid("security directory has no parent".to_owned())
            })?)
        }
        Err(error) => Err(SecurityError::Io(error.to_string())),
    }
}

#[cfg(not(any(unix, windows)))]
fn ensure_private_directory(_path: &Path) -> Result<(), SecurityError> {
    Err(SecurityError::Invalid(
        "workspace security is supported on Unix and Windows".to_owned(),
    ))
}

#[cfg(any(unix, windows))]
fn write_armored_file(
    directory: &Path,
    destination: &Path,
    password: &MasterPassword,
    kind: EnvelopeKind,
    logical_name: &str,
    payload: &[u8],
) -> Result<(), SecurityError> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    if destination.parent() != Some(directory)
        || destination.components().any(|component| {
            !matches!(
                component,
                Component::Normal(_) | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(SecurityError::Invalid(
            "security destination is invalid".to_owned(),
        ));
    }
    let temporary = directory.join(format!(
        ".stillus-security-{}-{:016x}.tmp",
        std::process::id(),
        TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let output = options.open(&temporary).map_err(security_io)?;
    let metadata = EnvelopeMetadata::new(kind, logical_name.to_owned(), payload.len() as u64)
        .map_err(|_| SecurityError::Invalid("security envelope metadata is invalid".to_owned()))?;
    let mut writer = armored_writer(output, password, metadata)?;
    if let Err(error) = writer.write_all(payload) {
        let _ = fs::remove_file(&temporary);
        return Err(SecurityError::Io(error.to_string()));
    }
    let mut output = writer
        .finish()
        .map_err(|_| SecurityError::AuthenticationFailed)?;
    output.flush().map_err(security_io)?;
    output.sync_all().map_err(security_io)?;
    drop(output);
    match fs::hard_link(&temporary, destination) {
        Ok(()) => {}
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(if error.kind() == io::ErrorKind::AlreadyExists {
                SecurityError::Conflict(format!("{} already exists", destination.display()))
            } else {
                SecurityError::Io(error.to_string())
            });
        }
    }
    fs::remove_file(&temporary).map_err(security_io)?;
    sync_directory(directory)?;
    let metadata = fs::symlink_metadata(destination).map_err(security_io)?;
    validate_file(destination, &metadata)
}

#[cfg(not(any(unix, windows)))]
fn write_armored_file(
    _directory: &Path,
    _destination: &Path,
    _password: &MasterPassword,
    _kind: EnvelopeKind,
    _logical_name: &str,
    _payload: &[u8],
) -> Result<(), SecurityError> {
    Err(SecurityError::Invalid(
        "workspace security is supported on Unix and Windows".to_owned(),
    ))
}

fn read_armored_payload(
    path: &Path,
    password: &MasterPassword,
    kind: EnvelopeKind,
) -> Result<Vec<u8>, SecurityError> {
    let file = File::open(path).map_err(security_io)?;
    let reader =
        decrypt_armored(file, password, kind).map_err(|_| SecurityError::AuthenticationFailed)?;
    let expected = usize::try_from(reader.metadata().payload_len)
        .map_err(|_| SecurityError::AuthenticationFailed)?;
    if expected > MAX_SECURITY_PAYLOAD {
        return Err(SecurityError::Blocked(
            "security payload is too large".to_owned(),
        ));
    }
    let mut payload = Vec::with_capacity(expected);
    reader
        .take((MAX_SECURITY_PAYLOAD + 1) as u64)
        .read_to_end(&mut payload)
        .map_err(|_| SecurityError::AuthenticationFailed)?;
    if payload.len() != expected {
        return Err(SecurityError::AuthenticationFailed);
    }
    Ok(payload)
}

fn armored_writer<W: Write>(
    output: W,
    password: &MasterPassword,
    metadata: EnvelopeMetadata,
) -> Result<ArmoredEnvelopeWriter<W>, SecurityError> {
    #[cfg(any(test, feature = "test-utils"))]
    {
        ArmoredEnvelopeWriter::new_for_test(output, password, metadata)
            .map_err(|_| SecurityError::AuthenticationFailed)
    }
    #[cfg(not(any(test, feature = "test-utils")))]
    {
        ArmoredEnvelopeWriter::new(output, password, metadata)
            .map_err(|_| SecurityError::AuthenticationFailed)
    }
}

fn sync_directory(path: &Path) -> Result<(), SecurityError> {
    stillus_platform::sync_directory(path).map_err(security_io)
}

fn security_io(error: io::Error) -> SecurityError {
    SecurityError::Io(error.to_string())
}

fn random_hex(bytes: usize) -> String {
    let mut value = vec![0_u8; bytes];
    OsRng.fill_bytes(&mut value);
    let mut output = String::with_capacity(bytes * 2);
    for byte in value {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn is_hex_id(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_key(value: &str) -> Result<(), SecurityError> {
    if value.is_empty()
        || value.starts_with('/')
        || value.ends_with('/')
        || value.split('/').any(|part| {
            part.is_empty()
                || part == "."
                || part == ".."
                || part
                    .chars()
                    .any(|character| character.is_control() || character == '\\')
        })
    {
        return Err(SecurityError::Invalid(
            "secret field key is invalid".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    struct TestWorkspace(PathBuf);

    impl TestWorkspace {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "stillus-security-test-{}-{}",
                std::process::id(),
                TEST_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn binding(field: &str) -> SecretBinding {
        SecretBinding::new(
            EngineId::new("test").unwrap(),
            ItemId::new("items/one").unwrap(),
            field,
        )
        .unwrap()
    }

    #[test]
    fn verifier_is_armored_private_and_authenticates_without_notes() {
        let workspace = TestWorkspace::new();
        let store = SecurityStore::new(&workspace.0);
        let password = MasterPassword::new("correct password".to_owned());
        let vault = store.configure(&password).unwrap();
        assert_eq!(store.unlock(&password).unwrap(), vault);
        assert!(matches!(
            store.unlock(&MasterPassword::new("wrong password".to_owned())),
            Err(SecurityError::AuthenticationFailed)
        ));
        assert!(
            fs::read(store.verifier_path())
                .unwrap()
                .starts_with(stillus_secure::ARMORED_AGE_PREFIX)
        );
        assert_eq!(
            store.inspect(false).unwrap().state,
            WorkspaceSecurityState::ConfiguredLocked
        );
    }

    #[test]
    fn verifier_accepts_windows_armor_without_rewriting_it() {
        let workspace = TestWorkspace::new();
        let store = SecurityStore::new(&workspace.0);
        let password = MasterPassword::new("portable verifier".to_owned());
        let vault = store.configure(&password).unwrap();
        let path = store.verifier_path();
        let crlf = fs::read_to_string(&path).unwrap().replace('\n', "\r\n");
        fs::write(&path, &crlf).unwrap();
        assert_eq!(store.unlock(&password).unwrap(), vault);
        assert_eq!(fs::read_to_string(&path).unwrap(), crlf);
    }

    #[test]
    fn secret_ciphertext_is_bound_to_workspace_owner_and_field() {
        let first = TestWorkspace::new();
        let second = TestWorkspace::new();
        let password = MasterPassword::new("shared password".to_owned());
        let first_store = SecurityStore::new(&first.0);
        let second_store = SecurityStore::new(&second.0);
        first_store.configure(&password).unwrap();
        second_store.configure(&password).unwrap();
        let value = SecretValue::new(b"engine-token".to_vec()).unwrap();
        let reference = first_store
            .store_secret(&binding("connection/token"), &value, &password)
            .unwrap();
        assert_eq!(
            first_store
                .resolve_secret(&reference, &binding("connection/token"), &password)
                .unwrap()
                .expose(),
            b"engine-token"
        );
        assert!(matches!(
            first_store.resolve_secret(&reference, &binding("connection/other"), &password),
            Err(SecurityError::AuthenticationFailed)
        ));
        fs::copy(
            first_store.secret_path(&reference),
            second_store.secret_path(&reference),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                second_store.secret_path(&reference),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
        assert!(matches!(
            second_store.resolve_secret(&reference, &binding("connection/token"), &password),
            Err(SecurityError::AuthenticationFailed)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn linked_verifier_and_missing_verifier_with_secrets_block_the_store() {
        let workspace = TestWorkspace::new();
        let store = SecurityStore::new(&workspace.0);
        let password = MasterPassword::new("password".to_owned());
        store.configure(&password).unwrap();
        let linked = workspace.0.join("linked.age");
        fs::hard_link(store.verifier_path(), &linked).unwrap();
        assert!(matches!(
            store.inspect(false),
            Err(SecurityError::Blocked(_))
        ));
        fs::remove_file(linked).unwrap();
        let value = SecretValue::new(b"value".to_vec()).unwrap();
        store
            .store_secret(&binding("token"), &value, &password)
            .unwrap();
        fs::remove_file(store.verifier_path()).unwrap();
        assert!(matches!(
            store.inspect(false),
            Err(SecurityError::Blocked(_))
        ));
    }
}
