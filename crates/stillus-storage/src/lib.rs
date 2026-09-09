// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

mod password_change;
mod permanent_delete;
#[cfg(any(windows, test))]
mod replace_retry;
mod secure_backups;

pub use password_change::{
    PasswordChangeCommit, PasswordChangeError, PasswordChangePhase, PasswordChangeProgress,
    PasswordChangeTarget, SecurityRotationCommit, SecurityRotationError, SecurityRotationPhase,
    SecurityRotationProgress, SecurityRotationTarget, SecurityRotationTargets,
    change_master_password, recover_password_change, rotate_workspace_security,
};
pub use permanent_delete::delete_trashed_note_versioned;
pub use secure_backups::{
    IntegrityFailure, SecureBackupRecord, VerifiedSave, load_pending_integrity_failure,
    restore_secure_backup,
};

use std::ffi::OsString;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use stillus_platform::fs::{self, File, OpenOptions};

use pulldown_cmark::{Event, Parser};
use stillus_frontmatter::{
    EncryptionFormat, EncryptionPatch, FrontMatterScan, FrontMatterStatus, MAX_FRONT_MATTER_BYTES,
    MetadataPatch, PatchError, patch_front_matter, scan_reader,
};
use stillus_secure::{
    AGE_PREFIX, ARMORED_AGE_CRLF_PREFIX, ARMORED_AGE_PREFIX, BodyEnvelopeWriter, EnvelopeKind,
    EnvelopeMetadata, EnvelopeWriter, MasterPassword, decrypt, decrypt_body, is_age_prefix,
    is_armored_age_prefix, is_scrypt_age_envelope, opaque_note_filename,
};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

const COPY_BUFFER_BYTES: usize = 65_536;
const MAX_NOTE_TITLE_CHARS: usize = 120;
const MAX_NOTE_TITLE_BYTES: usize = 240;
pub const BODY_TITLE_SCAN_BYTES: usize = 8 * 1024;
pub const BODY_TITLE_SCAN_LINES: usize = 32;
pub const EMPTY_NOTE_TITLE: &str = "Новая заметка";
const MAX_TAG_CHARS: usize = 120;
const MAX_TAG_BYTES: usize = 240;
#[cfg(any(unix, windows))]
const SECURE_TEMP_STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
#[cfg(any(unix, windows))]
#[cfg(unix)]
const PROTECTION_JOURNAL_MAGIC: &[u8] = b"NTRMLOCKJOURNAL2\n";
#[cfg(windows)]
const PROTECTION_JOURNAL_MAGIC: &[u8] = b"NTRMLOCKJOURNALWINDOWS2\n";
#[cfg(any(unix, windows))]
const SECURE_TEMP_NAME_BYTES: usize = ".ntrm-secure-".len() + 32 + ".tmp".len();
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NoteScan {
    pub path: PathBuf,
    pub result: NoteScanResult,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NoteScanResult {
    Scanned(ScannedNote),
    Protected(ScannedProtectedNote),
    LegacyProtected,
    InvalidProtected(String),
    IoError(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScannedNote {
    pub frontmatter: FrontMatterScan,
    pub body_title: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScannedProtectedNote {
    pub frontmatter: FrontMatterScan,
    pub body_offset: u64,
    pub version: FileVersion,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceScan {
    pub notes: Vec<NoteScan>,
}

pub fn initialize_workspace(workspace: impl AsRef<Path>) -> io::Result<()> {
    let workspace = workspace.as_ref();
    ensure_workspace_directory(workspace)?;
    let notes = workspace.join("notes");
    match fs::symlink_metadata(&notes) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("notes path must be a real directory: {}", notes.display()),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => match fs::create_dir(&notes) {
            Ok(()) => sync_directory_io(workspace)?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                validate_real_notes_directory(&notes)?;
            }
            Err(error) => return Err(error),
        },
        Err(error) => return Err(error),
    }
    validate_real_notes_directory(&notes)
}

fn ensure_workspace_directory(workspace: &Path) -> io::Result<()> {
    match fs::symlink_metadata(workspace) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "workspace path must be a real directory: {}",
                workspace.display()
            ),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = workspace.parent().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "workspace has no parent directory",
                )
            })?;
            let parent_metadata = fs::symlink_metadata(parent)?;
            if !parent_metadata.file_type().is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "workspace parent must be a real directory: {}",
                        parent.display()
                    ),
                ));
            }
            match fs::create_dir(workspace) {
                Ok(()) => sync_directory_io(parent),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    ensure_workspace_directory(workspace)
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

pub fn scan_workspace(workspace: impl AsRef<Path>) -> io::Result<WorkspaceScan> {
    let notes_directory = workspace.as_ref().join("notes");
    validate_real_notes_directory(&notes_directory)?;
    let mut paths = Vec::new();
    for entry in fs::read_dir(notes_directory)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_file()
            || entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "md")
        {
            continue;
        }
        paths.push(entry.path());
    }
    paths.sort();

    let notes = paths
        .into_iter()
        .map(|path| {
            let result =
                scan_note(&path).unwrap_or_else(|error| NoteScanResult::IoError(error.to_string()));
            NoteScan { path, result }
        })
        .collect();
    Ok(WorkspaceScan { notes })
}

pub fn scan_note(path: impl AsRef<Path>) -> io::Result<NoteScanResult> {
    let path = path.as_ref();
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "note path must be a regular file and not a symlink",
        ));
    }
    let mut file = File::open(path)?;
    let version = FileVersion::from_metadata(&file.metadata()?);
    let mut prefix = vec![0_u8; ARMORED_AGE_CRLF_PREFIX.len().max(AGE_PREFIX.len())];
    let mut read = 0;
    while read < prefix.len() {
        let count = file.read(&mut prefix[read..])?;
        if count == 0 {
            break;
        }
        read += count;
    }
    if read >= AGE_PREFIX.len() && is_age_prefix(&prefix[..read]) {
        file.seek(SeekFrom::Start(0))?;
        if is_scrypt_age_envelope(&mut file) {
            return Ok(
                if path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(is_opaque_note_name)
                {
                    NoteScanResult::LegacyProtected
                } else {
                    NoteScanResult::InvalidProtected(
                        "binary age data at the start of a Markdown note is unsupported".to_owned(),
                    )
                },
            );
        }
    }
    if read >= ARMORED_AGE_PREFIX.len() && is_armored_age_prefix(&prefix[..read]) {
        return Ok(NoteScanResult::InvalidProtected(
            "armored age body is missing the protected-note marker".to_owned(),
        ));
    }
    file.seek(SeekFrom::Start(0))?;
    let frontmatter = scan_reader(&mut file)?;
    if let FrontMatterStatus::Parsed(parsed) = &frontmatter.status {
        let body_offset = parsed.body_offset;
        file.seek(SeekFrom::Start(body_offset))?;
        let mut armored_prefix = vec![0_u8; ARMORED_AGE_CRLF_PREFIX.len()];
        let mut armored_read = 0;
        while armored_read < armored_prefix.len() {
            let count = file.read(&mut armored_prefix[armored_read..])?;
            if count == 0 {
                break;
            }
            armored_read += count;
        }
        let has_armor = is_armored_age_prefix(&armored_prefix[..armored_read]);
        return match (parsed.metadata.encryption, has_armor) {
            (Some(EncryptionFormat::AgeBodyV1), true) => {
                Ok(NoteScanResult::Protected(ScannedProtectedNote {
                    frontmatter,
                    body_offset,
                    version,
                }))
            }
            (Some(EncryptionFormat::AgeBodyV1), false) => Ok(NoteScanResult::InvalidProtected(
                "protected-note marker is not followed by an armored age body".to_owned(),
            )),
            (None, true) => Ok(NoteScanResult::InvalidProtected(
                "armored age body is missing the protected-note marker".to_owned(),
            )),
            (None, false) => scan_plain_note(file, frontmatter, Some(body_offset)),
        };
    }
    if let FrontMatterStatus::Invalid { body_offset, .. } = &frontmatter.status {
        let mut looks_protected = false;
        if let Some(body_offset) = body_offset {
            file.seek(SeekFrom::Start(*body_offset))?;
            let mut armor = vec![0_u8; ARMORED_AGE_CRLF_PREFIX.len()];
            looks_protected = file.read_exact(&mut armor).is_ok() && is_armored_age_prefix(&armor);
        }
        file.seek(SeekFrom::Start(0))?;
        let mut header = vec![0_u8; frontmatter.bytes_read.min(MAX_FRONT_MATTER_BYTES)];
        file.read_exact(&mut header)?;
        looks_protected |= header
            .split(|byte| *byte == b'\n')
            .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
            .any(|line| line.starts_with(b"stillus_encryption:"));
        if looks_protected {
            return Ok(NoteScanResult::InvalidProtected(
                "invalid protected-note front matter".to_owned(),
            ));
        }
    }
    scan_plain_note(file, frontmatter, None)
}

fn scan_plain_note(
    mut file: File,
    frontmatter: FrontMatterScan,
    parsed_body_offset: Option<u64>,
) -> io::Result<NoteScanResult> {
    let body_offset = match &frontmatter.status {
        FrontMatterStatus::Plain => Some(0),
        FrontMatterStatus::Parsed(parsed) => parsed_body_offset.or(Some(parsed.body_offset)),
        FrontMatterStatus::Invalid { .. } => None,
    };
    let body_title = body_offset
        .map(|offset| scan_body_title(&mut file, offset))
        .transpose()?
        .flatten();
    Ok(NoteScanResult::Scanned(ScannedNote {
        frontmatter,
        body_title,
    }))
}

fn scan_body_title(file: &mut File, body_offset: u64) -> io::Result<Option<String>> {
    file.seek(SeekFrom::Start(body_offset))?;
    let mut bytes = Vec::with_capacity(BODY_TITLE_SCAN_BYTES + 1);
    file.take((BODY_TITLE_SCAN_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    let definitely_eof = bytes.len() <= BODY_TITLE_SCAN_BYTES;
    let bounded = &bytes[..bytes.len().min(BODY_TITLE_SCAN_BYTES)];
    let (text, definitely_eof) = match std::str::from_utf8(bounded) {
        Ok(text) => (text, definitely_eof),
        Err(error) if error.error_len().is_none() => {
            let prefix = &bounded[..error.valid_up_to()];
            (std::str::from_utf8(prefix).unwrap_or(""), false)
        }
        Err(_) => return Ok(None),
    };
    Ok(project_body_title_bounded(text, definitely_eof))
}

fn project_body_title_bounded(body: &str, definitely_eof: bool) -> Option<String> {
    let mut start = 0;
    for line_index in 0..BODY_TITLE_SCAN_LINES {
        let remainder = &body[start..];
        let newline = remainder.find('\n');
        let end = newline.map_or(body.len(), |relative| start + relative);
        let line = body[start..end]
            .strip_suffix('\r')
            .unwrap_or(&body[start..end]);
        if !line.trim().is_empty() {
            let complete = newline.is_some() || definitely_eof;
            return complete.then(|| project_markdown_title(line)).flatten();
        }
        let relative = newline?;
        start = start.saturating_add(relative).saturating_add(1);
        if start >= body.len() {
            return None;
        }
        if line_index + 1 == BODY_TITLE_SCAN_LINES {
            return None;
        }
    }
    None
}

/// Returns the visible text of the first non-empty Markdown line within the
/// same bounds used by workspace scanning. Callers with an in-memory body use
/// this function so scan, core and search share one title projection.
pub fn project_body_title(body: &str) -> Option<String> {
    let bounded_end = floor_char_boundary(body, body.len().min(BODY_TITLE_SCAN_BYTES));
    let definitely_eof = body.len() <= BODY_TITLE_SCAN_BYTES;
    project_body_title_bounded(&body[..bounded_end], definitely_eof)
}

pub fn project_markdown_title(line: &str) -> Option<String> {
    let mut visible = String::new();
    for event in Parser::new(line) {
        match event {
            Event::Text(text) | Event::Code(text) => visible.push_str(&text),
            Event::SoftBreak | Event::HardBreak => visible.push(' '),
            _ => {}
        }
    }
    let visible = visible
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let visible = visible.trim();
    (!visible.is_empty()).then(|| visible.to_owned())
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

pub fn repair_workspace(workspace: impl AsRef<Path>) -> io::Result<()> {
    let _operation = stillus_platform::OperationLock::directory(workspace.as_ref())?;
    validate_real_notes_directory(&workspace.as_ref().join("notes"))
}

fn validate_real_notes_directory(notes_directory: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(notes_directory)?;
    if !metadata.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "notes path must be a real directory",
        ));
    }
    Ok(())
}

#[allow(dead_code)]
fn repair_protected_names(notes_directory: &Path) -> io::Result<()> {
    validate_real_notes_directory(notes_directory)?;
    #[cfg(any(unix, windows))]
    {
        let journals = fs::read_dir(notes_directory)?
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.file_type().is_ok_and(|kind| kind.is_file())
                    && entry
                        .file_name()
                        .to_str()
                        .is_some_and(is_protection_journal_name)
            })
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        for path in journals {
            let Some(journal) = read_protection_journal(&path)? else {
                continue;
            };
            repair_protection_transition(notes_directory, &path, &journal)?;
        }
    }
    Ok(())
}

pub fn cleanup_stale_secure_temps(workspace: impl AsRef<Path>) -> io::Result<usize> {
    let _operation = stillus_platform::OperationLock::directory(workspace.as_ref())?;
    let notes_directory = workspace.as_ref().join("notes");
    let directory_metadata = fs::symlink_metadata(&notes_directory)?;
    if !directory_metadata.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "notes path must be a real directory",
        ));
    }
    // Whole-file protection journals belong to the unsupported legacy format.
    // Startup must leave them and any referenced files byte-for-byte intact.
    Ok(0)
}

#[cfg(any(unix, windows))]
fn is_stale_secure_temp(metadata: &fs::Metadata) -> bool {
    metadata
        .modified()
        .ok()
        .and_then(|modified| std::time::SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age >= SECURE_TEMP_STALE_AFTER)
}

#[cfg(any(unix, windows))]
fn is_owned_unpublished_secure_temp(
    path: &Path,
    metadata: &fs::Metadata,
    journal: &ProtectionJournal,
) -> io::Result<bool> {
    if !metadata.file_type().is_file()
        || metadata.nlink() != 1
        || metadata.dev() != journal.device
        || metadata.ino() != journal.inode
        || metadata.len() != journal.size
        || metadata.ctime() != journal.changed_seconds
        || metadata.ctime_nsec() != journal.changed_nanoseconds
        || path.file_name().and_then(|name| name.to_str()) != Some(journal.temp_name.as_str())
    {
        return Ok(false);
    }
    path_has_age_prefix(path)
}

#[cfg(any(unix, windows))]
fn path_matches_protection_journal(path: &Path, journal: &ProtectionJournal) -> io::Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file()
        || metadata.dev() != journal.device
        || metadata.ino() != journal.inode
        || metadata.len() != journal.size
    {
        return Ok(false);
    }
    path_has_age_prefix(path)
}

fn is_secure_temp_name(value: &str) -> bool {
    value
        .strip_prefix(".ntrm-secure-")
        .and_then(|value| value.strip_suffix(".tmp"))
        .is_some_and(|identifier| {
            identifier.len() == 32
                && identifier
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
}

fn is_opaque_note_name(value: &str) -> bool {
    opaque_note_parts(value).is_some()
}

fn opaque_note_parts(value: &str) -> Option<(bool, &str)> {
    let (deleted, identifier) = if let Some(identifier) = value
        .strip_prefix("ntrm-deleted-")
        .and_then(|value| value.strip_suffix(".md"))
    {
        (true, identifier)
    } else {
        (false, value.strip_prefix("ntrm-")?.strip_suffix(".md")?)
    };
    (identifier.len() == 32
        && identifier
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
    .then_some((deleted, identifier))
}

#[cfg(any(unix, windows))]
#[derive(Clone, Debug, Eq, PartialEq)]
struct ProtectionJournal {
    device: u64,
    inode: u64,
    size: u64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
    temp_name: String,
    destination_name: String,
}

#[cfg(any(unix, windows))]
fn protection_journal_name(destination_name: &str) -> Option<String> {
    destination_name
        .strip_prefix("ntrm-")
        .and_then(|name| name.strip_suffix(".md"))
        .filter(|identifier| {
            identifier.len() == 32
                && identifier
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
        .map(|identifier| format!(".ntrm-transition-{identifier}.journal"))
}

#[cfg(any(unix, windows))]
fn is_protection_journal_name(value: &str) -> bool {
    value
        .strip_prefix(".ntrm-transition-")
        .and_then(|value| value.strip_suffix(".journal"))
        .is_some_and(|identifier| {
            identifier.len() == 32
                && identifier
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
}

#[cfg(any(unix, windows))]
fn create_protection_journal(
    notes_directory: &Path,
    temp_path: &Path,
    encrypted_metadata: &fs::Metadata,
    destination: &Path,
) -> io::Result<TempGuard> {
    let temp_name = temp_path
        .parent()
        .filter(|parent| *parent == notes_directory)
        .and_then(|_| temp_path.file_name())
        .and_then(|name| name.to_str())
        .filter(|name| is_secure_temp_name(name))
        .ok_or_else(|| io::Error::other("invalid protected-note temp"))?;
    let destination_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| is_opaque_note_name(name))
        .ok_or_else(|| io::Error::other("invalid protected-note destination"))?;
    let journal_name = protection_journal_name(destination_name)
        .ok_or_else(|| io::Error::other("invalid protection-journal destination"))?;
    let path = notes_directory.join(journal_name);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file = options.open(&path)?;
    let mut guard = TempGuard::new(path);
    let write_result = (|| {
        file.write_all(PROTECTION_JOURNAL_MAGIC)?;
        file.write_all(&encrypted_metadata.dev().to_le_bytes())?;
        file.write_all(&encrypted_metadata.ino().to_le_bytes())?;
        file.write_all(&encrypted_metadata.len().to_le_bytes())?;
        file.write_all(&encrypted_metadata.ctime().to_le_bytes())?;
        file.write_all(&encrypted_metadata.ctime_nsec().to_le_bytes())?;
        file.write_all(temp_name.as_bytes())?;
        file.write_all(destination_name.as_bytes())?;
        file.flush()?;
        file.sync_all()?;
        sync_directory_io(notes_directory)
    })();
    if let Err(error) = write_result {
        drop(file);
        let _ = fs::remove_file(guard.path());
        let _ = sync_directory_io(notes_directory);
        guard.disarm();
        return Err(error);
    }
    Ok(guard)
}

#[cfg(any(unix, windows))]
fn read_protection_journal(path: &Path) -> io::Result<Option<ProtectionJournal>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.nlink() != 1 {
        return Ok(None);
    }
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(None);
    };
    if !is_protection_journal_name(file_name) {
        return Ok(None);
    }
    let expected_len =
        PROTECTION_JOURNAL_MAGIC.len() + 8 + 8 + 8 + 8 + 8 + SECURE_TEMP_NAME_BYTES + 40;
    if metadata.len() != expected_len as u64 {
        return Ok(None);
    }
    let mut bytes = vec![0_u8; expected_len];
    File::open(path)?.read_exact(&mut bytes)?;
    if !bytes.starts_with(PROTECTION_JOURNAL_MAGIC) {
        return Ok(None);
    }
    let mut offset = PROTECTION_JOURNAL_MAGIC.len();
    let device = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;
    let inode = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;
    let size = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;
    let changed_seconds = i64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;
    let changed_nanoseconds = i64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;
    let Ok(temp_name) = std::str::from_utf8(&bytes[offset..offset + SECURE_TEMP_NAME_BYTES]) else {
        return Ok(None);
    };
    offset += SECURE_TEMP_NAME_BYTES;
    let Ok(destination_name) = std::str::from_utf8(&bytes[offset..]) else {
        return Ok(None);
    };
    if !is_secure_temp_name(temp_name)
        || !is_opaque_note_name(destination_name)
        || protection_journal_name(destination_name).as_deref() != Some(file_name)
    {
        return Ok(None);
    }
    Ok(Some(ProtectionJournal {
        device,
        inode,
        size,
        changed_seconds,
        changed_nanoseconds,
        temp_name: temp_name.to_owned(),
        destination_name: destination_name.to_owned(),
    }))
}

#[cfg(any(unix, windows))]
fn repair_protection_transition(
    notes_directory: &Path,
    journal_path: &Path,
    journal: &ProtectionJournal,
) -> io::Result<()> {
    let destination = notes_directory.join(&journal.destination_name);
    let mut matching = Vec::new();
    for entry in fs::read_dir(notes_directory)? {
        let entry = entry?;
        if entry.path() == journal_path {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_file()
            && metadata.dev() == journal.device
            && metadata.ino() == journal.inode
        {
            matching.push(entry.path());
        }
    }

    let mut destination_matches = path_matches_protection_journal(&destination, journal)?;
    if fs::symlink_metadata(&destination).is_ok() && !destination_matches {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "protection journal destination is occupied by another inode",
        ));
    }
    let non_opaque_notes = matching
        .iter()
        .filter(|path| {
            path.extension().is_some_and(|extension| extension == "md")
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| !is_opaque_note_name(name))
        })
        .cloned()
        .collect::<Vec<_>>();
    if non_opaque_notes.len() > 1 {
        return Err(io::Error::other(
            "protection journal matches multiple non-opaque notes",
        ));
    }

    if let Some(source) = non_opaque_notes.first() {
        let source_matches = path_matches_protection_journal(source, journal)?;
        if !source_matches && !destination_matches {
            return Err(io::Error::other(
                "protection journal source is not an age envelope",
            ));
        }
        if source_matches && !destination_matches {
            match fs::hard_link(source, &destination) {
                Ok(()) => destination_matches = true,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::AlreadyExists | io::ErrorKind::NotFound
                    ) && path_matches_protection_journal(&destination, journal)? =>
                {
                    destination_matches = true;
                }
                Err(error) => return Err(error),
            }
            sync_directory_io(notes_directory)?;
        }
        if source_matches {
            match fs::remove_file(source) {
                Ok(()) => sync_directory_io(notes_directory)?,
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound
                        && path_matches_protection_journal(&destination, journal)? => {}
                Err(error) => return Err(error),
            }
        }
    } else if !destination_matches {
        let temp = matching.iter().find(|path| {
            path.file_name().and_then(|name| name.to_str()) == Some(journal.temp_name.as_str())
        });
        if let Some(temp) = temp {
            let metadata = fs::symlink_metadata(temp)?;
            if !is_owned_unpublished_secure_temp(temp, &metadata, journal)? {
                return Ok(());
            }
            if !is_stale_secure_temp(&metadata) {
                // A fresh journal/temp pair may belong to an in-flight lock
                // transaction in this process. Startup cleanup will reclaim a
                // crashed pre-publication pair only after the stale threshold.
                return Ok(());
            }
            // The journal reached disk before canonical publication. The
            // original plaintext note is still authoritative, so the proven
            // app-owned encrypted temp can be discarded safely.
            fs::remove_file(temp)?;
            sync_directory_io(notes_directory)?;
        } else if !matching.is_empty() {
            // A matching inode under an unexpected name is not enough proof of
            // ownership. Preserve both it and the journal for manual review.
            return Ok(());
        }
    }

    match fs::remove_file(journal_path) {
        Ok(()) => sync_directory_io(notes_directory),
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                && destination_matches
                && path_matches_protection_journal(&destination, journal)? =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[cfg(any(unix, windows))]
fn path_has_age_prefix(path: &Path) -> io::Result<bool> {
    let mut file = File::open(path)?;
    let mut prefix = vec![0_u8; AGE_PREFIX.len()];
    match file.read_exact(&mut prefix) {
        Ok(()) => Ok(is_age_prefix(&prefix)),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(error),
    }
}

pub fn parsed_notes(scan: &WorkspaceScan) -> impl Iterator<Item = &NoteScan> {
    scan.notes.iter().filter(|note| {
        matches!(
            note.result,
            NoteScanResult::Scanned(ScannedNote {
                frontmatter: FrontMatterScan {
                    status: FrontMatterStatus::Parsed(_),
                    ..
                },
                ..
            })
        )
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SaveStage {
    OpenTarget,
    Scan,
    CreateTemp,
    Write,
    FileSync,
    ConflictCheck,
    Replace,
    SourceRemove,
    ParentSync,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SaveOutcome {
    Unchanged,
    Committed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SaveCommit {
    pub outcome: SaveOutcome,
    pub version: FileVersion,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SaveError {
    UnsupportedPlatform,
    InvalidTarget(String),
    Patch(PatchError),
    Conflict,
    PreCommit { stage: SaveStage, message: String },
    PostReplaceSync { message: String },
    PartialCommit { path: PathBuf, message: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationStage {
    Validate,
    CreateDirectory,
    Write,
    FileSync,
    Publish,
    SourceRemove,
    DirectorySync,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NoteOperationError {
    InvalidName(String),
    InvalidTag(String),
    InvalidWorkspace(String),
    Collision(PathBuf),
    Conflict,
    Save(SaveError),
    Failed {
        stage: OperationStage,
        message: String,
    },
    PartialCommit {
        message: String,
    },
}

impl std::fmt::Display for NoteOperationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName(message) => write!(formatter, "invalid note name: {message}"),
            Self::InvalidTag(message) => write!(formatter, "invalid tag: {message}"),
            Self::InvalidWorkspace(message) => write!(formatter, "invalid workspace: {message}"),
            Self::Collision(path) => {
                write!(formatter, "destination already exists: {}", path.display())
            }
            Self::Conflict => formatter.write_str("source changed during note operation"),
            Self::Save(error) => write!(formatter, "note rewrite failed: {error}"),
            Self::Failed { stage, message } => {
                write!(formatter, "note operation failed at {stage:?}: {message}")
            }
            Self::PartialCommit { message } => {
                write!(
                    formatter,
                    "note operation left recoverable partial state: {message}"
                )
            }
        }
    }
}

impl std::error::Error for NoteOperationError {}

impl From<SaveError> for NoteOperationError {
    fn from(error: SaveError) -> Self {
        match error {
            SaveError::Conflict => Self::Conflict,
            error => Self::Save(error),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NoteCommit {
    pub path: PathBuf,
    pub version: FileVersion,
}

/// Moves an encrypted canonical note between the active and deleted opaque
/// filename namespaces without exposing encrypted metadata or overwriting an
/// existing path.
pub fn relocate_protected_note_state(
    workspace: impl AsRef<Path>,
    source: impl AsRef<Path>,
    expected_version: &FileVersion,
    deleted: bool,
) -> Result<NoteCommit, NoteOperationError> {
    let _operation = stillus_platform::OperationLock::directory(workspace.as_ref())
        .map_err(|error| SaveError::InvalidTarget(error.to_string()))?;
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (workspace, source, expected_version, deleted);
        return Err(NoteOperationError::Save(SaveError::UnsupportedPlatform));
    }

    #[cfg(any(unix, windows))]
    {
        let notes_directory = direct_notes_directory(workspace.as_ref())?;
        let source = source.as_ref();
        validate_direct_note(source, &notes_directory)?;
        let source_name = source
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                NoteOperationError::InvalidName(
                    "protected note filename must be valid UTF-8".to_owned(),
                )
            })?;
        let (currently_deleted, identifier) = opaque_note_parts(source_name).ok_or_else(|| {
            NoteOperationError::InvalidName(
                "protected source must have an opaque note filename".to_owned(),
            )
        })?;
        let source_metadata = fs::symlink_metadata(source)
            .map_err(|error| operation_failure(OperationStage::Validate, error))?;
        if FileVersion::from_metadata(&source_metadata) != *expected_version {
            return Err(NoteOperationError::Conflict);
        }
        if currently_deleted == deleted {
            return Ok(NoteCommit {
                path: source.to_path_buf(),
                version: FileVersion::from_metadata(&source_metadata),
            });
        }
        let destination_name = if deleted {
            format!("ntrm-deleted-{identifier}.md")
        } else {
            format!("ntrm-{identifier}.md")
        };
        let destination = notes_directory.join(destination_name);
        ensure_destination_available(&notes_directory, &destination, Some(source))?;
        fs::hard_link(source, &destination)
            .map_err(|error| operation_failure(OperationStage::Publish, error))?;
        if let Err(error) = sync_directory_io(&notes_directory) {
            let _ = fs::remove_file(&destination);
            let _ = sync_directory_io(&notes_directory);
            return Err(operation_failure(OperationStage::DirectorySync, error));
        }
        let current_metadata =
            fs::symlink_metadata(source).map_err(|error| NoteOperationError::PartialCommit {
                message: format!("protected source could not be rechecked: {error}"),
            })?;
        let current_version = FileVersion::from_metadata(&current_metadata);
        if !current_metadata.file_type().is_file()
            || !current_version.same_content_as(expected_version)
        {
            let _ = fs::remove_file(&destination);
            let _ = sync_directory_io(&notes_directory);
            return Err(NoteOperationError::Conflict);
        }
        fs::remove_file(source).map_err(|error| NoteOperationError::PartialCommit {
            message: format!("protected state committed before old-name cleanup: {error}"),
        })?;
        sync_directory_io(&notes_directory).map_err(|error| NoteOperationError::PartialCommit {
            message: format!("protected state relocation committed before directory sync: {error}"),
        })?;
        let metadata = fs::symlink_metadata(&destination).map_err(|error| {
            NoteOperationError::PartialCommit {
                message: format!("protected state destination could not be inspected: {error}"),
            }
        })?;
        Ok(NoteCommit {
            path: destination,
            version: FileVersion::from_metadata(&metadata),
        })
    }
}

/// Replaces a plaintext canonical note with a self-contained age envelope and
/// relocates it to an opaque filename. Before the atomic replace every error
/// leaves the plaintext source unchanged. After it, errors are reported as an
/// explicit partial commit because rolling back to plaintext would violate the
/// protection request.
pub fn protect_note(
    workspace: impl AsRef<Path>,
    source: impl AsRef<Path>,
    expected_version: &FileVersion,
    password: &MasterPassword,
) -> Result<NoteCommit, NoteOperationError> {
    let _operation = stillus_platform::OperationLock::directory(workspace.as_ref())
        .map_err(|error| SaveError::InvalidTarget(error.to_string()))?;
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (workspace, source, expected_version, password);
        return Err(NoteOperationError::Save(SaveError::UnsupportedPlatform));
    }

    #[cfg(any(unix, windows))]
    {
        let mut checkpoint = NoOperationFault;
        protect_note_with(
            workspace.as_ref(),
            source.as_ref(),
            expected_version,
            password,
            &mut checkpoint,
        )
    }
}

#[cfg(any(unix, windows))]
fn protect_note_with(
    workspace: &Path,
    source: &Path,
    expected_version: &FileVersion,
    password: &MasterPassword,
    checkpoint: &mut impl OperationCheckpoint,
) -> Result<NoteCommit, NoteOperationError> {
    checkpoint.check(OperationStage::Validate)?;
    let notes_directory = direct_notes_directory(workspace)?;
    validate_direct_note(source, &notes_directory)?;
    let source_metadata = fs::symlink_metadata(source)
        .map_err(|error| operation_failure(OperationStage::Validate, error))?;
    if FileVersion::from_metadata(&source_metadata) != *expected_version {
        return Err(NoteOperationError::Conflict);
    }
    let original_filename = source
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            NoteOperationError::InvalidName(
                "protected note filename must be valid UTF-8".to_owned(),
            )
        })?
        .to_owned();
    let payload_len = source_metadata.len();
    let envelope_metadata =
        EnvelopeMetadata::new(EnvelopeKind::Note, original_filename, payload_len)
            .map_err(|error| operation_failure(OperationStage::Validate, error))?;
    let destination = allocate_opaque_destination(&notes_directory)
        .map_err(|error| operation_failure(OperationStage::Validate, error))?;

    checkpoint.check(OperationStage::Write)?;
    let (mut guard, temp) = create_secure_temp(&notes_directory)
        .map_err(|error| operation_failure(OperationStage::Write, error))?;
    temp.set_permissions(source_metadata.permissions())
        .map_err(|error| operation_failure(OperationStage::Write, error))?;
    let mut encrypted = create_envelope_writer(temp, password, envelope_metadata)
        .map_err(|error| operation_failure(OperationStage::Write, error))?;
    let mut input =
        File::open(source).map_err(|error| operation_failure(OperationStage::Validate, error))?;
    let opened_version = input
        .metadata()
        .map(|metadata| FileVersion::from_metadata(&metadata))
        .map_err(|error| operation_failure(OperationStage::Validate, error))?;
    if opened_version != *expected_version {
        return Err(NoteOperationError::Conflict);
    }
    copy_bounded(&mut input, &mut encrypted)
        .map_err(|error| operation_failure(OperationStage::Write, error))?;
    drop(input);
    let mut temp = encrypted
        .finish()
        .map_err(|error| operation_failure(OperationStage::Write, error))?;
    temp.flush()
        .map_err(|error| operation_failure(OperationStage::Write, error))?;
    checkpoint.check(OperationStage::FileSync)?;
    temp.sync_all()
        .map_err(|error| operation_failure(OperationStage::FileSync, error))?;

    let encrypted_metadata = temp
        .metadata()
        .map_err(|error| operation_failure(OperationStage::Validate, error))?;
    checkpoint.check(OperationStage::Write)?;
    let mut journal_guard = create_protection_journal(
        &notes_directory,
        guard.path(),
        &encrypted_metadata,
        &destination,
    )
    .map_err(|error| operation_failure(OperationStage::Write, error))?;
    let journal_path = journal_guard.path().to_path_buf();
    let journal = read_protection_journal(&journal_path)
        .map_err(|error| operation_failure(OperationStage::Write, error))?
        .ok_or_else(|| {
            operation_failure(
                OperationStage::Write,
                io::Error::other("created protection journal failed validation"),
            )
        })?;
    checkpoint.check(OperationStage::FileSync)?;

    let current_metadata = fs::symlink_metadata(source)
        .map_err(|error| operation_failure(OperationStage::Validate, error))?;
    if !current_metadata.file_type().is_file()
        || FileVersion::from_metadata(&current_metadata) != *expected_version
    {
        return Err(NoteOperationError::Conflict);
    }

    drop(temp);
    checkpoint.check(OperationStage::Publish)?;
    journal_guard.disarm();
    if let Err(error) = fs::rename(guard.path(), source) {
        let _ = fs::remove_file(&journal_path);
        let _ = sync_directory_io(&notes_directory);
        return Err(operation_failure(OperationStage::Publish, error));
    }
    guard.disarm();
    if let Err(error) = checkpoint
        .check(OperationStage::DirectorySync)
        .and_then(|()| {
            sync_directory_io(&notes_directory)
                .map_err(|error| operation_failure(OperationStage::DirectorySync, error))
        })
    {
        return Err(NoteOperationError::PartialCommit {
            message: format!("encrypted content committed before directory sync: {error}"),
        });
    }

    if let Err(error) = checkpoint.check(OperationStage::Publish) {
        return Err(NoteOperationError::PartialCommit {
            message: format!("encrypted note awaits opaque relocation: {error}"),
        });
    }
    match fs::hard_link(source, &destination) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::AlreadyExists | io::ErrorKind::NotFound
            ) && path_matches_protection_journal(&destination, &journal).map_err(
                |verify_error| NoteOperationError::PartialCommit {
                    message: format!(
                        "encrypted note relocation could not be verified: {verify_error}"
                    ),
                },
            )? => {}
        Err(error) => {
            return Err(NoteOperationError::PartialCommit {
                message: format!("encrypted note awaits opaque relocation: {error}"),
            });
        }
    }
    if let Err(error) = checkpoint
        .check(OperationStage::DirectorySync)
        .and_then(|()| {
            sync_directory_io(&notes_directory)
                .map_err(|error| operation_failure(OperationStage::DirectorySync, error))
        })
    {
        return Err(NoteOperationError::PartialCommit {
            message: format!("opaque encrypted link could not be synced: {error}"),
        });
    }
    if let Err(error) = checkpoint.check(OperationStage::SourceRemove) {
        return Err(NoteOperationError::PartialCommit {
            message: format!("encrypted note awaits old-name cleanup: {error}"),
        });
    }
    match fs::symlink_metadata(source) {
        Ok(_) => {
            let source_matches =
                path_matches_protection_journal(source, &journal).map_err(|error| {
                    NoteOperationError::PartialCommit {
                        message: format!(
                            "encrypted old-name cleanup could not be verified: {error}"
                        ),
                    }
                })?;
            if !source_matches {
                return Err(NoteOperationError::PartialCommit {
                    message: "encrypted old-name path no longer matches the committed inode"
                        .to_owned(),
                });
            }
            match fs::remove_file(source) {
                Ok(()) => {}
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound
                        && path_matches_protection_journal(&destination, &journal).map_err(
                            |verify_error| NoteOperationError::PartialCommit {
                                message: format!(
                                    "encrypted destination could not be verified: {verify_error}"
                                ),
                            },
                        )? => {}
                Err(error) => {
                    return Err(NoteOperationError::PartialCommit {
                        message: format!("encrypted note awaits old-name cleanup: {error}"),
                    });
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if !path_matches_protection_journal(&destination, &journal).map_err(|verify_error| {
                NoteOperationError::PartialCommit {
                    message: format!("encrypted destination could not be verified: {verify_error}"),
                }
            })? {
                return Err(NoteOperationError::PartialCommit {
                    message: "encrypted old-name path disappeared without a matching opaque note"
                        .to_owned(),
                });
            }
        }
        Err(error) => {
            return Err(NoteOperationError::PartialCommit {
                message: format!("encrypted old-name cleanup could not be inspected: {error}"),
            });
        }
    }
    if let Err(error) = checkpoint
        .check(OperationStage::DirectorySync)
        .and_then(|()| {
            sync_directory_io(&notes_directory)
                .map_err(|error| operation_failure(OperationStage::DirectorySync, error))
        })
    {
        return Err(NoteOperationError::PartialCommit {
            message: format!("opaque relocation committed before final directory sync: {error}"),
        });
    }
    let metadata =
        fs::symlink_metadata(&destination).map_err(|error| NoteOperationError::PartialCommit {
            message: format!("opaque encrypted note metadata unavailable: {error}"),
        })?;
    if !path_matches_protection_journal(&destination, &journal).map_err(|error| {
        NoteOperationError::PartialCommit {
            message: format!("opaque encrypted note could not be verified: {error}"),
        }
    })? {
        return Err(NoteOperationError::PartialCommit {
            message: "opaque encrypted note does not match the committed inode".to_owned(),
        });
    }
    match fs::remove_file(&journal_path) {
        Ok(()) => {
            if let Err(error) = sync_directory_io(&notes_directory) {
                return Err(NoteOperationError::PartialCommit {
                    message: format!("protected note committed before journal sync: {error}"),
                });
            }
        }
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                && path_matches_protection_journal(&destination, &journal).map_err(
                    |verify_error| NoteOperationError::PartialCommit {
                        message: format!(
                            "missing journal destination could not be verified: {verify_error}"
                        ),
                    },
                )? => {}
        Err(error) => {
            return Err(NoteOperationError::PartialCommit {
                message: format!("protected note committed before journal cleanup: {error}"),
            });
        }
    }
    Ok(NoteCommit {
        path: destination,
        version: FileVersion::from_metadata(&metadata),
    })
}

/// Authenticates and streams a protected note back to its original plaintext
/// filename without overwriting any existing note. Errors before publication
/// leave the encrypted canonical file unchanged; errors after publication are
/// reported as an explicit partial commit.
pub fn disable_protection(
    workspace: impl AsRef<Path>,
    source: impl AsRef<Path>,
    expected_version: &FileVersion,
    password: &MasterPassword,
) -> Result<NoteCommit, NoteOperationError> {
    let _operation = stillus_platform::OperationLock::directory(workspace.as_ref())
        .map_err(|error| SaveError::InvalidTarget(error.to_string()))?;
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (workspace, source, expected_version, password);
        return Err(NoteOperationError::Save(SaveError::UnsupportedPlatform));
    }

    #[cfg(any(unix, windows))]
    {
        let mut checkpoint = NoOperationFault;
        disable_protection_with(
            workspace.as_ref(),
            source.as_ref(),
            expected_version,
            password,
            &mut checkpoint,
        )
    }
}

#[cfg(any(unix, windows))]
fn disable_protection_with(
    workspace: &Path,
    source: &Path,
    expected_version: &FileVersion,
    password: &MasterPassword,
    checkpoint: &mut impl OperationCheckpoint,
) -> Result<NoteCommit, NoteOperationError> {
    let notes_directory = direct_notes_directory(workspace)?;
    validate_direct_note(source, &notes_directory)?;
    if !source
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_opaque_note_name)
    {
        return Err(NoteOperationError::InvalidWorkspace(
            "protected source must have an opaque note filename".to_owned(),
        ));
    }

    let source_metadata = fs::symlink_metadata(source)
        .map_err(|error| operation_failure(OperationStage::Validate, error))?;
    if FileVersion::from_metadata(&source_metadata) != *expected_version {
        return Err(NoteOperationError::Conflict);
    }
    let input =
        File::open(source).map_err(|error| operation_failure(OperationStage::Validate, error))?;
    let opened_version = input
        .metadata()
        .map(|metadata| FileVersion::from_metadata(&metadata))
        .map_err(|error| operation_failure(OperationStage::Validate, error))?;
    if opened_version != *expected_version {
        return Err(NoteOperationError::Conflict);
    }

    let mut decrypted = decrypt(input, password, EnvelopeKind::Note)
        .map_err(|error| operation_failure(OperationStage::Validate, error))?;
    let original_filename = decrypted.metadata().original_filename.clone();
    let destination = notes_directory.join(&original_filename);
    if destination.parent() != Some(notes_directory.as_path())
        || destination
            .extension()
            .is_none_or(|extension| extension != "md")
    {
        return Err(NoteOperationError::InvalidName(
            "protected note contains an invalid original filename".to_owned(),
        ));
    }

    checkpoint.check(OperationStage::Write)?;
    let (mut guard, mut temp) = create_secure_temp(&notes_directory)
        .map_err(|error| operation_failure(OperationStage::Write, error))?;
    temp.set_permissions(source_metadata.permissions())
        .map_err(|error| operation_failure(OperationStage::Write, error))?;
    copy_bounded(&mut decrypted, &mut temp)
        .map_err(|error| operation_failure(OperationStage::Write, error))?;
    drop(decrypted);
    temp.flush()
        .map_err(|error| operation_failure(OperationStage::Write, error))?;
    checkpoint.check(OperationStage::FileSync)?;
    temp.sync_all()
        .map_err(|error| operation_failure(OperationStage::FileSync, error))?;

    let current_metadata = fs::symlink_metadata(source)
        .map_err(|error| operation_failure(OperationStage::Validate, error))?;
    if !current_metadata.file_type().is_file()
        || FileVersion::from_metadata(&current_metadata) != *expected_version
    {
        return Err(NoteOperationError::Conflict);
    }
    ensure_destination_available(&notes_directory, &destination, Some(source))?;

    checkpoint.check(OperationStage::Publish)?;
    drop(temp);
    publish_temp(&mut guard, &destination)?;

    if let Err(error) = checkpoint.check(OperationStage::DirectorySync) {
        return Err(partial_disable("plaintext publish directory sync", error));
    }
    if let Err(error) = sync_directory_io(&notes_directory) {
        return Err(partial_disable("plaintext publish directory sync", error));
    }
    if let Err(error) = checkpoint.check(OperationStage::SourceRemove) {
        return Err(partial_disable("encrypted source removal", error));
    }
    let removal_metadata = fs::symlink_metadata(source)
        .map_err(|error| partial_disable("encrypted source identity check", error))?;
    if !removal_metadata.file_type().is_file()
        || FileVersion::from_metadata(&removal_metadata) != *expected_version
    {
        return Err(NoteOperationError::PartialCommit {
            message: "disable protection published plaintext, but encrypted source changed before removal"
                .to_owned(),
        });
    }
    if let Err(error) = fs::remove_file(source) {
        return Err(partial_disable("encrypted source removal", error));
    }
    if let Err(error) = checkpoint.check(OperationStage::DirectorySync) {
        return Err(partial_disable("final directory sync", error));
    }
    if let Err(error) = sync_directory_io(&notes_directory) {
        return Err(partial_disable("final directory sync", error));
    }
    let metadata = fs::symlink_metadata(&destination)
        .map_err(|error| partial_disable("plaintext metadata read", error))?;
    Ok(NoteCommit {
        path: destination,
        version: FileVersion::from_metadata(&metadata),
    })
}

fn partial_disable(stage: &str, error: impl std::fmt::Display) -> NoteOperationError {
    NoteOperationError::PartialCommit {
        message: format!("disable protection committed before {stage}: {error}"),
    }
}

pub fn rewrite_protected_note(
    path: impl AsRef<Path>,
    expected_version: &FileVersion,
    password: &MasterPassword,
    original_filename: &str,
    markdown_prefix: &[u8],
    body_len: u64,
    write_body: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> Result<SaveCommit, SaveError> {
    let _operation = stillus_platform::OperationLock::file(path.as_ref())
        .map_err(|error| SaveError::InvalidTarget(error.to_string()))?;
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (
            path,
            expected_version,
            password,
            original_filename,
            markdown_prefix,
            body_len,
            write_body,
        );
        return Err(SaveError::UnsupportedPlatform);
    }

    #[cfg(any(unix, windows))]
    rewrite_protected_note_unix(
        path.as_ref(),
        expected_version,
        password,
        original_filename,
        markdown_prefix,
        body_len,
        write_body,
    )
}

/// Converts a regular Markdown note into the v1 body-only protected format.
/// The YAML front matter stays in plaintext and the canonical path is not
/// changed by the protection operation.
pub fn protect_note_body(
    path: impl AsRef<Path>,
    expected_version: &FileVersion,
    password: &MasterPassword,
    title: &str,
) -> Result<SaveCommit, SaveError> {
    let _operation = stillus_platform::OperationLock::file(path.as_ref())
        .map_err(|error| SaveError::InvalidTarget(error.to_string()))?;
    let path = path.as_ref();
    let (mut input, opened_version) = open_versioned(path)?;
    if opened_version != *expected_version {
        return Err(SaveError::Conflict);
    }
    let metadata = input
        .metadata()
        .map_err(|error| precommit(SaveStage::OpenTarget, error))?;
    let scan = scan_reader(&mut input).map_err(|error| precommit(SaveStage::Scan, error))?;
    let body_offset = match &scan.status {
        FrontMatterStatus::Plain => 0,
        FrontMatterStatus::Parsed(parsed) if parsed.metadata.encryption.is_none() => {
            parsed.body_offset
        }
        FrontMatterStatus::Parsed(_) => {
            return Err(SaveError::InvalidTarget(
                "note is already protected".to_owned(),
            ));
        }
        FrontMatterStatus::Invalid { issue, .. } => {
            return Err(SaveError::Patch(PatchError::InvalidFrontMatter(
                issue.clone(),
            )));
        }
    };
    let body_len = metadata
        .len()
        .checked_sub(body_offset)
        .ok_or_else(|| SaveError::InvalidTarget("invalid note body offset".to_owned()))?;
    input
        .seek(SeekFrom::Start(body_offset))
        .map_err(|error| precommit(SaveStage::Scan, error))?;
    let password = password.clone();
    rewrite_note(
        path,
        expected_version,
        &MetadataPatch {
            title: Some(title.to_owned()),
            encryption: EncryptionPatch::Set(EncryptionFormat::AgeBodyV1),
            ..MetadataPatch::default()
        },
        move |output| {
            let mut encrypted = create_body_envelope_writer(output, &password, body_len)
                .map_err(io::Error::other)?;
            copy_bounded(&mut input, &mut encrypted)?;
            encrypted.finish().map_err(io::Error::other)?;
            Ok(())
        },
    )
}

/// Re-encrypts an in-memory body snapshot and applies the ordinary
/// title-derived collision-safe filename policy.
pub struct ProtectedBodyRewrite<'a> {
    pub password: &'a MasterPassword,
    pub patch: &'a MetadataPatch,
    pub title: &'a str,
    pub body_len: u64,
}

pub fn rewrite_protected_body_with_title(
    workspace: impl AsRef<Path>,
    path: impl AsRef<Path>,
    expected_version: &FileVersion,
    request: ProtectedBodyRewrite<'_>,
    write_body: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> Result<VerifiedSave, SaveError> {
    let _operation = stillus_platform::OperationLock::directory(workspace.as_ref())
        .map_err(|error| SaveError::InvalidTarget(error.to_string()))?;
    let password = request.password.clone();
    let workspace = workspace.as_ref();
    let path = path.as_ref();
    rewrite_existing_protected_with_title(
        workspace,
        path,
        expected_version,
        request.patch,
        Some(request.title),
        move |output| {
            let mut encrypted = create_body_envelope_writer(output, &password, request.body_len)
                .map_err(io::Error::other)?;
            write_body(&mut encrypted)?;
            encrypted.finish().map_err(io::Error::other)?;
            Ok(())
        },
    )
}

/// Removes body-only protection while preserving the current canonical path
/// and every front-matter field except the Stillus encryption marker.
pub fn disable_body_protection(
    workspace: impl AsRef<Path>,
    path: impl AsRef<Path>,
    expected_version: &FileVersion,
    password: &MasterPassword,
    title: &str,
) -> Result<VerifiedSave, SaveError> {
    let _operation = stillus_platform::OperationLock::directory(workspace.as_ref())
        .map_err(|error| SaveError::InvalidTarget(error.to_string()))?;
    let workspace = workspace.as_ref();
    let path = path.as_ref();
    let (mut input, opened_version) = open_versioned(path)?;
    if opened_version != *expected_version {
        return Err(SaveError::Conflict);
    }
    let scan = scan_reader(&mut input).map_err(|error| precommit(SaveStage::Scan, error))?;
    let body_offset = match &scan.status {
        FrontMatterStatus::Parsed(parsed)
            if parsed.metadata.encryption == Some(EncryptionFormat::AgeBodyV1) =>
        {
            parsed.body_offset
        }
        _ => {
            return Err(SaveError::InvalidTarget(
                "note is not a body-only protected note".to_owned(),
            ));
        }
    };
    input
        .seek(SeekFrom::Start(body_offset))
        .map_err(|error| precommit(SaveStage::Scan, error))?;
    let password = password.clone();
    rewrite_existing_protected_with_title(
        workspace,
        path,
        expected_version,
        &MetadataPatch {
            encryption: EncryptionPatch::Remove,
            ..MetadataPatch::default()
        },
        Some(title),
        move |output| {
            let mut decrypted = decrypt_body(input, &password).map_err(io::Error::other)?;
            copy_bounded(&mut decrypted, output)?;
            Ok(())
        },
    )
}

/// Applies a metadata-only change to an existing protected note while keeping
/// the armored body byte-identical and retaining a verified rollback copy.
pub fn rewrite_protected_metadata_versioned(
    workspace: impl AsRef<Path>,
    path: impl AsRef<Path>,
    expected_version: &FileVersion,
    patch: &MetadataPatch,
    rename_title: Option<&str>,
) -> Result<VerifiedSave, SaveError> {
    let _operation = stillus_platform::OperationLock::directory(workspace.as_ref())
        .map_err(|error| SaveError::InvalidTarget(error.to_string()))?;
    if patch.is_empty() {
        return Err(SaveError::InvalidTarget(
            "protected metadata rewrite requires a non-empty patch".to_owned(),
        ));
    }
    let workspace = workspace.as_ref();
    let path = path.as_ref();
    let (mut input, opened_version) = open_versioned(path)?;
    if opened_version != *expected_version {
        return Err(SaveError::Conflict);
    }
    let scan = scan_reader(&mut input).map_err(|error| precommit(SaveStage::Scan, error))?;
    let body_offset = match &scan.status {
        FrontMatterStatus::Parsed(parsed)
            if parsed.metadata.encryption == Some(EncryptionFormat::AgeBodyV1) =>
        {
            parsed.body_offset
        }
        _ => {
            return Err(SaveError::InvalidTarget(
                "metadata target is not a protected note".to_owned(),
            ));
        }
    };
    input
        .seek(SeekFrom::Start(body_offset))
        .map_err(|error| precommit(SaveStage::Scan, error))?;
    rewrite_existing_protected_with_title(
        workspace,
        path,
        expected_version,
        patch,
        rename_title,
        move |writer| copy_bounded(&mut input, writer).map(|_| ()),
    )
}

fn rewrite_existing_protected_with_title(
    workspace: &Path,
    path: &Path,
    expected_version: &FileVersion,
    patch: &MetadataPatch,
    title: Option<&str>,
    write_body: impl FnOnce(&mut File) -> io::Result<()>,
) -> Result<VerifiedSave, SaveError> {
    let backup = secure_backups::prepare_backup(workspace, path, expected_version)?;
    let notes = direct_notes_directory(workspace)
        .map_err(|error| SaveError::InvalidTarget(error.to_string()))?;
    validate_direct_note(path, &notes)
        .map_err(|error| SaveError::InvalidTarget(error.to_string()))?;
    let destination = match title {
        Some(title) => available_title_path(&notes, title, Some(path))
            .map_err(|error| SaveError::InvalidTarget(error.to_string()))?,
        None => path.to_path_buf(),
    };
    let (commit, expected_sha256) = rewrite_note_to_destination_internal(
        path,
        &destination,
        expected_version,
        patch,
        write_body,
        true,
    )?;
    let expected_sha256 = expected_sha256.ok_or_else(|| {
        SaveError::InvalidTarget("protected rewrite did not produce a hash".to_owned())
    })?;
    secure_backups::verify_commit(workspace, commit, backup, expected_sha256)
}

#[cfg(any(unix, windows))]
fn rewrite_protected_note_unix(
    path: &Path,
    expected_version: &FileVersion,
    password: &MasterPassword,
    original_filename: &str,
    markdown_prefix: &[u8],
    body_len: u64,
    write_body: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> Result<SaveCommit, SaveError> {
    let target_metadata =
        fs::symlink_metadata(path).map_err(|error| precommit(SaveStage::OpenTarget, error))?;
    if !target_metadata.file_type().is_file()
        || FileVersion::from_metadata(&target_metadata) != *expected_version
    {
        return Err(SaveError::Conflict);
    }
    let prefix_len =
        u64::try_from(markdown_prefix.len()).map_err(|error| precommit(SaveStage::Write, error))?;
    let payload_len = prefix_len
        .checked_add(body_len)
        .ok_or_else(|| SaveError::InvalidTarget("protected note is too large".to_owned()))?;
    let metadata = EnvelopeMetadata::new(
        EnvelopeKind::Note,
        original_filename.to_owned(),
        payload_len,
    )
    .map_err(|error| precommit(SaveStage::Write, error))?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let (mut guard, temp) =
        create_secure_temp(parent).map_err(|error| precommit(SaveStage::CreateTemp, error))?;
    temp.set_permissions(target_metadata.permissions())
        .map_err(|error| precommit(SaveStage::CreateTemp, error))?;
    let mut encrypted = create_envelope_writer(temp, password, metadata)
        .map_err(|error| precommit(SaveStage::Write, error))?;
    encrypted
        .write_all(markdown_prefix)
        .map_err(|error| precommit(SaveStage::Write, error))?;
    write_body(&mut encrypted).map_err(|error| precommit(SaveStage::Write, error))?;
    let mut temp = encrypted
        .finish()
        .map_err(|error| precommit(SaveStage::Write, error))?;
    temp.flush()
        .map_err(|error| precommit(SaveStage::Write, error))?;
    temp.sync_all()
        .map_err(|error| precommit(SaveStage::FileSync, error))?;

    let current_metadata =
        fs::symlink_metadata(path).map_err(|error| precommit(SaveStage::ConflictCheck, error))?;
    if !current_metadata.file_type().is_file()
        || FileVersion::from_metadata(&current_metadata) != *expected_version
    {
        return Err(SaveError::Conflict);
    }
    drop(temp);
    fs::rename(guard.path(), path).map_err(|error| precommit(SaveStage::Replace, error))?;
    guard.disarm();
    sync_directory_io(parent).map_err(|error| SaveError::PostReplaceSync {
        message: error.to_string(),
    })?;
    let committed_metadata =
        fs::symlink_metadata(path).map_err(|error| SaveError::PostReplaceSync {
            message: error.to_string(),
        })?;
    Ok(SaveCommit {
        outcome: SaveOutcome::Committed,
        version: FileVersion::from_metadata(&committed_metadata),
        path: path.to_path_buf(),
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrashCommit {
    pub original_path: PathBuf,
    pub trash_path: PathBuf,
}

pub fn validate_note_title(value: &str) -> Result<String, NoteOperationError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(NoteOperationError::InvalidName(
            "name must not be empty".to_owned(),
        ));
    }
    if value.chars().count() > MAX_NOTE_TITLE_CHARS || value.len() > MAX_NOTE_TITLE_BYTES {
        return Err(NoteOperationError::InvalidName(format!(
            "name exceeds {MAX_NOTE_TITLE_CHARS} characters or {MAX_NOTE_TITLE_BYTES} bytes"
        )));
    }
    if matches!(value, "." | "..") || value.starts_with('.') {
        return Err(NoteOperationError::InvalidName(
            "dot and hidden names are not portable".to_owned(),
        ));
    }
    if value.ends_with([' ', '.']) {
        return Err(NoteOperationError::InvalidName(
            "name must not end with a space or dot".to_owned(),
        ));
    }
    if value
        .chars()
        .any(|character| character.is_control() || "/\\<>:\"|?*".contains(character))
    {
        return Err(NoteOperationError::InvalidName(
            "name contains a control, separator, or platform-reserved character".to_owned(),
        ));
    }
    let device_stem = value
        .split('.')
        .next()
        .unwrap_or(value)
        .to_ascii_uppercase();
    let reserved = matches!(device_stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || device_stem
            .strip_prefix("COM")
            .or_else(|| device_stem.strip_prefix("LPT"))
            .is_some_and(|suffix| suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9'));
    if reserved {
        return Err(NoteOperationError::InvalidName(
            "name is reserved by a supported platform".to_owned(),
        ));
    }
    Ok(value.to_owned())
}

/// Produces a portable filename stem without changing the user-facing title.
/// The optional collision suffix is included inside the 120-char/240-byte cap.
pub fn safe_note_filename_stem(title: &str, collision: usize) -> String {
    let mut cleaned = String::new();
    for character in title.trim().chars() {
        match character {
            '/' => cleaned.push('∕'),
            '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*' => cleaned.push(' '),
            character if character.is_control() => cleaned.push(' '),
            character => cleaned.push(character),
        }
    }
    let mut cleaned = cleaned.trim_matches([' ', '.']).to_owned();
    let device_stem = cleaned
        .split('.')
        .next()
        .unwrap_or(&cleaned)
        .to_ascii_uppercase();
    let reserved = matches!(device_stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || device_stem
            .strip_prefix("COM")
            .or_else(|| device_stem.strip_prefix("LPT"))
            .is_some_and(|suffix| suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9'));
    if cleaned.is_empty() || matches!(cleaned.as_str(), "." | "..") || reserved {
        cleaned = EMPTY_NOTE_TITLE.to_owned();
    }
    let suffix = if collision >= 2 {
        format!(" ({collision})")
    } else {
        String::new()
    };
    let char_limit = MAX_NOTE_TITLE_CHARS.saturating_sub(suffix.chars().count());
    let byte_limit = MAX_NOTE_TITLE_BYTES.saturating_sub(suffix.len());
    let mut end = 0;
    for (index, character) in cleaned.char_indices() {
        let next = index + character.len_utf8();
        if cleaned[..next].chars().count() > char_limit || next > byte_limit {
            break;
        }
        end = next;
    }
    let base = cleaned[..end].trim_end_matches([' ', '.']);
    let base = if base.is_empty() {
        EMPTY_NOTE_TITLE
    } else {
        base
    };
    format!("{base}{suffix}")
}

fn available_title_path(
    notes: &Path,
    title: &str,
    source: Option<&Path>,
) -> Result<PathBuf, NoteOperationError> {
    for collision in 1..10_000 {
        let stem = safe_note_filename_stem(title, collision);
        let candidate = notes.join(format!("{stem}.md"));
        match ensure_destination_available(notes, &candidate, source) {
            Ok(()) => return Ok(candidate),
            Err(NoteOperationError::Collision(_)) => {}
            Err(error) => return Err(error),
        }
    }
    Err(NoteOperationError::Collision(notes.join(format!(
        "{}.md",
        safe_note_filename_stem(title, 9_999)
    ))))
}

pub fn validate_tag(value: &str) -> Result<String, NoteOperationError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(NoteOperationError::InvalidTag(
            "tag must not be empty".to_owned(),
        ));
    }
    if value.chars().count() > MAX_TAG_CHARS || value.len() > MAX_TAG_BYTES {
        return Err(NoteOperationError::InvalidTag(format!(
            "tag exceeds {MAX_TAG_CHARS} characters or {MAX_TAG_BYTES} bytes"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(NoteOperationError::InvalidTag(
            "tag contains a control character".to_owned(),
        ));
    }
    Ok(value.to_owned())
}

pub fn create_note(
    workspace: impl AsRef<Path>,
    title: &str,
    timestamp: &str,
) -> Result<NoteCommit, NoteOperationError> {
    let _operation = stillus_platform::OperationLock::directory(workspace.as_ref())
        .map_err(|error| SaveError::InvalidTarget(error.to_string()))?;
    let mut checkpoint = NoOperationFault;
    create_note_with(workspace.as_ref(), title, timestamp, &mut checkpoint)
}

#[cfg(not(any(unix, windows)))]
fn create_note_with(
    _workspace: &Path,
    _title: &str,
    _timestamp: &str,
    _checkpoint: &mut impl OperationCheckpoint,
) -> Result<NoteCommit, NoteOperationError> {
    Err(NoteOperationError::Save(SaveError::UnsupportedPlatform))
}

#[cfg(any(unix, windows))]
fn create_note_with(
    workspace: &Path,
    title: &str,
    timestamp: &str,
    checkpoint: &mut impl OperationCheckpoint,
) -> Result<NoteCommit, NoteOperationError> {
    let title = validate_note_title(title)?;
    validate_scalar(timestamp, "timestamp")?;
    let notes_directory = direct_notes_directory(workspace)?;
    let destination = available_title_path(&notes_directory, &title, None)?;
    let (mut guard, mut temp) =
        create_temp(&destination, &notes_directory).map_err(operation_save)?;
    let quoted_title = yaml_quote(&title);
    let quoted_timestamp = yaml_quote(timestamp);
    let contents = format!(
        "---\nfavorited: false\npinned: false\ntags: []\ntitle: {quoted_title}\ncreated: {quoted_timestamp}\nmodified: {quoted_timestamp}\n---\n\n# {title}"
    );
    temp.write_all(contents.as_bytes())
        .map_err(|error| operation_failure(OperationStage::Write, error))?;
    temp.flush()
        .map_err(|error| operation_failure(OperationStage::Write, error))?;
    checkpoint.check(OperationStage::Write)?;
    temp.sync_all()
        .map_err(|error| operation_failure(OperationStage::FileSync, error))?;
    checkpoint.check(OperationStage::FileSync)?;
    drop(temp);
    checkpoint.check(OperationStage::Publish)?;
    publish_temp(&mut guard, &destination)?;
    if let Err(error) = sync_directory(&notes_directory, checkpoint) {
        rollback_published(&destination, &notes_directory, error.to_string())?;
        return Err(error);
    }
    let metadata = fs::symlink_metadata(&destination)
        .map_err(|error| operation_failure(OperationStage::DirectorySync, error))?;
    if !metadata.file_type().is_file() {
        return Err(NoteOperationError::PartialCommit {
            message: "published note is not a regular file".to_owned(),
        });
    }
    Ok(NoteCommit {
        path: destination,
        version: FileVersion::from_metadata(&metadata),
    })
}

pub fn rename_note(
    workspace: impl AsRef<Path>,
    source: impl AsRef<Path>,
    expected_version: &FileVersion,
    title: &str,
    timestamp: &str,
) -> Result<NoteCommit, NoteOperationError> {
    let _operation = stillus_platform::OperationLock::directory(workspace.as_ref())
        .map_err(|error| SaveError::InvalidTarget(error.to_string()))?;
    let mut checkpoint = NoOperationFault;
    rename_note_with(
        workspace.as_ref(),
        source.as_ref(),
        expected_version,
        title,
        timestamp,
        &mut checkpoint,
    )
}

#[cfg(not(any(unix, windows)))]
fn rename_note_with(
    _workspace: &Path,
    _source: &Path,
    _expected_version: &FileVersion,
    _title: &str,
    _timestamp: &str,
    _checkpoint: &mut impl OperationCheckpoint,
) -> Result<NoteCommit, NoteOperationError> {
    Err(NoteOperationError::Save(SaveError::UnsupportedPlatform))
}

#[cfg(any(unix, windows))]
fn rename_note_with(
    workspace: &Path,
    source: &Path,
    expected_version: &FileVersion,
    title: &str,
    timestamp: &str,
    checkpoint: &mut impl OperationCheckpoint,
) -> Result<NoteCommit, NoteOperationError> {
    rename_note_with_filesystem(
        workspace,
        source,
        expected_version,
        title,
        timestamp,
        checkpoint,
        &NativeRenameFilesystem,
    )
}

#[cfg(any(unix, windows))]
fn rename_note_with_filesystem(
    workspace: &Path,
    source: &Path,
    expected_version: &FileVersion,
    title: &str,
    timestamp: &str,
    checkpoint: &mut impl OperationCheckpoint,
    filesystem: &impl RenameFilesystem,
) -> Result<NoteCommit, NoteOperationError> {
    let title = validate_note_title(title)?;
    validate_scalar(timestamp, "timestamp")?;
    let notes_directory = direct_notes_directory(workspace)?;
    validate_direct_note(source, &notes_directory)?;
    let destination = notes_directory.join(format!("{title}.md"));
    let destination_aliases_source =
        destination != source && filesystem.destination_aliases_source(source, &destination)?;
    ensure_destination_available(&notes_directory, &destination, Some(source))?;

    let (mut input, opened_version) = open_versioned(source).map_err(NoteOperationError::from)?;
    if opened_version != *expected_version {
        return Err(NoteOperationError::Conflict);
    }
    let scan = scan_reader(&mut input)
        .map_err(|error| operation_failure(OperationStage::Validate, error))?;
    let rewrite = patch_front_matter(
        &scan,
        &MetadataPatch {
            title: Some(title.clone()),
            modified: Some(timestamp.to_owned()),
            ..MetadataPatch::default()
        },
    )
    .map_err(|error| NoteOperationError::Save(SaveError::Patch(error)))?
    .ok_or_else(|| NoteOperationError::InvalidName("rename patch is empty".to_owned()))?;
    input
        .seek(SeekFrom::Start(rewrite.body_offset))
        .map_err(|error| operation_failure(OperationStage::Validate, error))?;

    if destination == source {
        let commit = rewrite_note(
            source,
            expected_version,
            &MetadataPatch {
                title: Some(title),
                modified: Some(timestamp.to_owned()),
                ..MetadataPatch::default()
            },
            move |writer| copy_bounded(&mut input, writer).map(|_| ()),
        )?;
        return Ok(NoteCommit {
            path: destination,
            version: commit.version,
        });
    }

    if destination_aliases_source {
        checkpoint.check(OperationStage::Publish)?;
        let commit = rewrite_note(
            source,
            expected_version,
            &MetadataPatch {
                title: Some(title),
                modified: Some(timestamp.to_owned()),
                ..MetadataPatch::default()
            },
            move |writer| copy_bounded(&mut input, writer).map(|_| ()),
        )?;
        let current_metadata = fs::symlink_metadata(source).map_err(|error| {
            NoteOperationError::PartialCommit {
                message: format!(
                    "metadata was updated at {}, but the source cannot be revalidated before case-only rename: {error}",
                    source.display()
                ),
            }
        })?;
        if !current_metadata.file_type().is_file()
            || FileVersion::from_metadata(&current_metadata) != commit.version
        {
            return Err(NoteOperationError::PartialCommit {
                message: format!(
                    "metadata was updated at {}, but the source changed before case-only rename",
                    source.display()
                ),
            });
        }
        let still_aliases_source = filesystem
            .destination_aliases_source(source, &destination)
            .map_err(|error| NoteOperationError::PartialCommit {
                message: format!(
                    "metadata was updated at {}, but {} could not be revalidated before case-only rename: {error}",
                    source.display(),
                    destination.display()
                ),
            })?;
        if !still_aliases_source {
            return Err(NoteOperationError::PartialCommit {
                message: format!(
                    "metadata was updated at {}, but {} no longer resolves to the same file",
                    source.display(),
                    destination.display()
                ),
            });
        }
        filesystem.rename(source, &destination).map_err(|error| {
            NoteOperationError::PartialCommit {
                message: format!(
                    "metadata was updated at {}, but case-only rename to {} failed: {error}",
                    source.display(),
                    destination.display()
                ),
            }
        })?;
        if let Err(error) = sync_directory(&notes_directory, checkpoint) {
            return Err(NoteOperationError::PartialCommit {
                message: format!(
                    "case-only rename to {} committed, but directory sync failed: {error}",
                    destination.display()
                ),
            });
        }
        let metadata = fs::symlink_metadata(&destination).map_err(|error| {
            NoteOperationError::PartialCommit {
                message: format!(
                    "case-only rename to {} committed, but the destination cannot be inspected: {error}",
                    destination.display()
                ),
            }
        })?;
        let version = FileVersion::from_metadata(&metadata);
        if !metadata.file_type().is_file() || !version.same_file_as(&commit.version) {
            return Err(NoteOperationError::PartialCommit {
                message: format!(
                    "case-only rename to {} did not retain the atomically rewritten file",
                    destination.display()
                ),
            });
        }
        return Ok(NoteCommit {
            path: destination,
            version,
        });
    }

    let source_metadata = fs::symlink_metadata(source)
        .map_err(|error| operation_failure(OperationStage::Validate, error))?;
    let (mut guard, mut temp) =
        create_temp(&destination, &notes_directory).map_err(operation_save)?;
    temp.set_permissions(source_metadata.permissions())
        .map_err(|error| operation_failure(OperationStage::Write, error))?;
    temp.write_all(&rewrite.prefix)
        .map_err(|error| operation_failure(OperationStage::Write, error))?;
    copy_bounded(&mut input, &mut temp)
        .map_err(|error| operation_failure(OperationStage::Write, error))?;
    drop(input);
    temp.flush()
        .map_err(|error| operation_failure(OperationStage::Write, error))?;
    checkpoint.check(OperationStage::Write)?;
    temp.sync_all()
        .map_err(|error| operation_failure(OperationStage::FileSync, error))?;
    checkpoint.check(OperationStage::FileSync)?;
    let current_metadata = fs::symlink_metadata(source)
        .map_err(|error| operation_failure(OperationStage::Validate, error))?;
    if !current_metadata.file_type().is_file()
        || FileVersion::from_metadata(&current_metadata) != *expected_version
    {
        return Err(NoteOperationError::Conflict);
    }
    drop(temp);
    checkpoint.check(OperationStage::Publish)?;
    publish_temp(&mut guard, &destination)?;
    if let Err(error) = sync_directory(&notes_directory, checkpoint) {
        rollback_published(&destination, &notes_directory, error.to_string())?;
        return Err(error);
    }
    if let Err(error) = checkpoint.check(OperationStage::SourceRemove) {
        rollback_published(&destination, &notes_directory, error.to_string())?;
        return Err(error);
    }
    let before_remove = fs::symlink_metadata(source)
        .map_err(|error| operation_failure(OperationStage::SourceRemove, error))?;
    if !before_remove.file_type().is_file()
        || FileVersion::from_metadata(&before_remove) != *expected_version
    {
        rollback_published(
            &destination,
            &notes_directory,
            "source changed after destination publish".to_owned(),
        )?;
        return Err(NoteOperationError::Conflict);
    }
    if let Err(error) = fs::remove_file(source) {
        rollback_published(&destination, &notes_directory, error.to_string())?;
        return Err(operation_failure(OperationStage::SourceRemove, error));
    }
    if let Err(error) = sync_directory(&notes_directory, checkpoint) {
        return Err(NoteOperationError::PartialCommit {
            message: format!(
                "renamed note {} is complete, but directory sync failed: {error}",
                destination.display()
            ),
        });
    }
    let metadata = fs::symlink_metadata(&destination)
        .map_err(|error| operation_failure(OperationStage::DirectorySync, error))?;
    Ok(NoteCommit {
        path: destination,
        version: FileVersion::from_metadata(&metadata),
    })
}

#[cfg(any(unix, windows))]
trait RenameFilesystem {
    fn destination_aliases_source(
        &self,
        source: &Path,
        destination: &Path,
    ) -> Result<bool, NoteOperationError>;

    fn rename(&self, source: &Path, destination: &Path) -> io::Result<()>;
}

#[cfg(any(unix, windows))]
struct NativeRenameFilesystem;

#[cfg(any(unix, windows))]
impl RenameFilesystem for NativeRenameFilesystem {
    fn destination_aliases_source(
        &self,
        source: &Path,
        destination: &Path,
    ) -> Result<bool, NoteOperationError> {
        let source_metadata = fs::symlink_metadata(source)
            .map_err(|error| operation_failure(OperationStage::Validate, error))?;
        let destination_metadata = match fs::symlink_metadata(destination) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(operation_failure(OperationStage::Validate, error)),
        };
        Ok(source_metadata.file_type().is_file()
            && destination_metadata.file_type().is_file()
            && FileVersion::from_metadata(&source_metadata)
                .same_file_as(&FileVersion::from_metadata(&destination_metadata)))
    }

    fn rename(&self, source: &Path, destination: &Path) -> io::Result<()> {
        fs::rename(source, destination)
    }
}

pub fn trash_note(
    workspace: impl AsRef<Path>,
    source: impl AsRef<Path>,
    expected_version: &FileVersion,
) -> Result<TrashCommit, NoteOperationError> {
    let _operation = stillus_platform::OperationLock::directory(workspace.as_ref())
        .map_err(|error| SaveError::InvalidTarget(error.to_string()))?;
    let mut checkpoint = NoOperationFault;
    trash_note_with(
        workspace.as_ref(),
        source.as_ref(),
        expected_version,
        &mut checkpoint,
    )
}

#[cfg(not(any(unix, windows)))]
fn trash_note_with(
    _workspace: &Path,
    _source: &Path,
    _expected_version: &FileVersion,
    _checkpoint: &mut impl OperationCheckpoint,
) -> Result<TrashCommit, NoteOperationError> {
    Err(NoteOperationError::Save(SaveError::UnsupportedPlatform))
}

#[cfg(any(unix, windows))]
fn trash_note_with(
    workspace: &Path,
    source: &Path,
    expected_version: &FileVersion,
    checkpoint: &mut impl OperationCheckpoint,
) -> Result<TrashCommit, NoteOperationError> {
    let notes_directory = direct_notes_directory(workspace)?;
    validate_direct_note(source, &notes_directory)?;
    let source_metadata = fs::symlink_metadata(source)
        .map_err(|error| operation_failure(OperationStage::Validate, error))?;
    if FileVersion::from_metadata(&source_metadata) != *expected_version {
        return Err(NoteOperationError::Conflict);
    }
    let state_directory = ensure_real_directory(&workspace.join(".stillus"), checkpoint)?;
    let trash_directory = ensure_real_directory(&state_directory.join("trash"), checkpoint)?;
    let file_name = source
        .file_name()
        .ok_or_else(|| NoteOperationError::InvalidWorkspace("source has no filename".to_owned()))?;
    let destination = available_trash_path(&trash_directory, file_name)?;
    checkpoint.check(OperationStage::Publish)?;
    fs::hard_link(source, &destination).map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            NoteOperationError::Collision(destination.clone())
        } else {
            operation_failure(OperationStage::Publish, error)
        }
    })?;
    if let Err(error) = sync_directory(&trash_directory, checkpoint) {
        rollback_published(&destination, &trash_directory, error.to_string())?;
        return Err(error);
    }
    if let Err(error) = checkpoint.check(OperationStage::SourceRemove) {
        rollback_published(&destination, &trash_directory, error.to_string())?;
        return Err(error);
    }
    let before_remove = fs::symlink_metadata(source)
        .map_err(|error| operation_failure(OperationStage::SourceRemove, error))?;
    let current = FileVersion::from_metadata(&before_remove);
    if !before_remove.file_type().is_file() || !current.same_content_as(expected_version) {
        rollback_published(
            &destination,
            &trash_directory,
            "source changed after trash publish".to_owned(),
        )?;
        return Err(NoteOperationError::Conflict);
    }
    if let Err(error) = fs::remove_file(source) {
        rollback_published(&destination, &trash_directory, error.to_string())?;
        return Err(operation_failure(OperationStage::SourceRemove, error));
    }
    if let Err(error) = sync_directory(&notes_directory, checkpoint) {
        return Err(NoteOperationError::PartialCommit {
            message: format!(
                "trashed note {} is complete, but notes directory sync failed: {error}",
                destination.display()
            ),
        });
    }
    Ok(TrashCommit {
        original_path: source.to_path_buf(),
        trash_path: destination,
    })
}

fn validate_scalar(value: &str, field: &str) -> Result<(), NoteOperationError> {
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(NoteOperationError::InvalidName(format!(
            "{field} must be a non-empty single-line value"
        )));
    }
    Ok(())
}

fn yaml_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn direct_notes_directory(workspace: &Path) -> Result<PathBuf, NoteOperationError> {
    let notes = workspace.join("notes");
    let metadata = fs::symlink_metadata(&notes).map_err(|error| {
        NoteOperationError::InvalidWorkspace(format!(
            "notes directory {} is unavailable: {error}",
            notes.display()
        ))
    })?;
    if !metadata.file_type().is_dir() {
        return Err(NoteOperationError::InvalidWorkspace(format!(
            "notes path {} must be a real directory",
            notes.display()
        )));
    }
    Ok(notes)
}

fn validate_direct_note(source: &Path, notes: &Path) -> Result<(), NoteOperationError> {
    if source.parent() != Some(notes)
        || source.extension().is_none_or(|extension| extension != "md")
    {
        return Err(NoteOperationError::InvalidWorkspace(
            "source must be a direct notes/*.md path".to_owned(),
        ));
    }
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| operation_failure(OperationStage::Validate, error))?;
    if !metadata.file_type().is_file() {
        return Err(NoteOperationError::InvalidWorkspace(
            "source must be a regular file and not a symlink".to_owned(),
        ));
    }
    Ok(())
}

fn ensure_destination_available(
    notes: &Path,
    destination: &Path,
    source: Option<&Path>,
) -> Result<(), NoteOperationError> {
    let wanted = destination
        .file_name()
        .ok_or_else(|| NoteOperationError::InvalidName("destination has no filename".to_owned()))?
        .to_string_lossy()
        .to_lowercase();
    for entry in
        fs::read_dir(notes).map_err(|error| operation_failure(OperationStage::Validate, error))?
    {
        let entry = entry.map_err(|error| operation_failure(OperationStage::Validate, error))?;
        if source.is_some_and(|source| entry.path() == source) {
            continue;
        }
        if entry.file_name().to_string_lossy().to_lowercase() == wanted {
            return Err(NoteOperationError::Collision(entry.path()));
        }
    }
    Ok(())
}

fn ensure_real_directory(
    path: &Path,
    checkpoint: &mut impl OperationCheckpoint,
) -> Result<PathBuf, NoteOperationError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => return Ok(path.to_path_buf()),
        Ok(_) => {
            return Err(NoteOperationError::InvalidWorkspace(format!(
                "{} must be a real directory",
                path.display()
            )));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(operation_failure(OperationStage::CreateDirectory, error)),
    }
    checkpoint.check(OperationStage::CreateDirectory)?;
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(operation_failure(OperationStage::CreateDirectory, error)),
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| operation_failure(OperationStage::CreateDirectory, error))?;
    if !metadata.file_type().is_dir() {
        return Err(NoteOperationError::InvalidWorkspace(format!(
            "{} became a non-directory",
            path.display()
        )));
    }
    Ok(path.to_path_buf())
}

fn available_trash_path(
    trash: &Path,
    file_name: &std::ffi::OsStr,
) -> Result<PathBuf, NoteOperationError> {
    let original = Path::new(file_name);
    let stem = original
        .file_stem()
        .ok_or_else(|| NoteOperationError::InvalidName("trash source has no stem".to_owned()))?;
    let extension = original.extension();
    for suffix in 0..10_000_u32 {
        let candidate_name = if suffix == 0 {
            file_name.to_os_string()
        } else {
            let mut candidate = stem.to_os_string();
            candidate.push(format!(".{suffix}"));
            if let Some(extension) = extension {
                candidate.push(".");
                candidate.push(extension);
            }
            candidate
        };
        let candidate = trash.join(candidate_name);
        match fs::symlink_metadata(&candidate) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(candidate),
            Ok(_) => {}
            Err(error) => return Err(operation_failure(OperationStage::Validate, error)),
        }
    }
    Err(NoteOperationError::Collision(trash.join(file_name)))
}

#[cfg(any(unix, windows))]
fn publish_temp(guard: &mut TempGuard, destination: &Path) -> Result<(), NoteOperationError> {
    fs::hard_link(guard.path(), destination).map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            NoteOperationError::Collision(destination.to_path_buf())
        } else {
            operation_failure(OperationStage::Publish, error)
        }
    })?;
    if let Err(error) = fs::remove_file(guard.path()) {
        let rollback = fs::remove_file(destination);
        return match rollback {
            Ok(()) => Err(operation_failure(OperationStage::Publish, error)),
            Err(rollback_error) => Err(NoteOperationError::PartialCommit {
                message: format!(
                    "published {}, temp cleanup failed ({error}), rollback failed ({rollback_error})",
                    destination.display()
                ),
            }),
        };
    }
    guard.disarm();
    Ok(())
}

fn rollback_published(
    destination: &Path,
    directory: &Path,
    cause: String,
) -> Result<(), NoteOperationError> {
    if let Err(rollback_error) = fs::remove_file(destination) {
        return Err(NoteOperationError::PartialCommit {
            message: format!(
                "{} remains published after {cause}; rollback failed: {rollback_error}",
                destination.display()
            ),
        });
    }
    let _ = sync_directory_unchecked(directory);
    Ok(())
}

fn sync_directory(
    directory: &Path,
    checkpoint: &mut impl OperationCheckpoint,
) -> Result<(), NoteOperationError> {
    checkpoint.check(OperationStage::DirectorySync)?;
    sync_directory_unchecked(directory)
}

fn sync_directory_unchecked(directory: &Path) -> Result<(), NoteOperationError> {
    stillus_platform::sync_directory(directory)
        .map_err(|error| operation_failure(OperationStage::DirectorySync, error))
}

fn operation_save(error: SaveError) -> NoteOperationError {
    NoteOperationError::Save(error)
}

fn operation_failure(stage: OperationStage, error: impl std::fmt::Display) -> NoteOperationError {
    #[cfg(any(test, feature = "test-utils"))]
    eprintln!("NATIVE_OPERATION stage={stage:?} outcome=Failed");
    NoteOperationError::Failed {
        stage,
        message: error.to_string(),
    }
}

trait OperationCheckpoint {
    fn check(&mut self, stage: OperationStage) -> Result<(), NoteOperationError>;
}

struct NoOperationFault;

impl OperationCheckpoint for NoOperationFault {
    fn check(&mut self, _stage: OperationStage) -> Result<(), NoteOperationError> {
        Ok(())
    }
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                formatter.write_str("atomic metadata rewrite is not implemented on this platform")
            }
            Self::InvalidTarget(message) => write!(formatter, "invalid target: {message}"),
            Self::Patch(error) => write!(formatter, "front-matter patch failed: {error}"),
            Self::Conflict => formatter.write_str("target changed before atomic replace"),
            Self::PreCommit { stage, message } => {
                write!(
                    formatter,
                    "save failed before commit at {stage:?}: {message}"
                )
            }
            Self::PostReplaceSync { message } => write!(
                formatter,
                "target was replaced but parent-directory sync failed: {message}"
            ),
            Self::PartialCommit { path, message } => write!(
                formatter,
                "save committed at {}, but cleanup was incomplete: {message}",
                path.display()
            ),
        }
    }
}

pub fn rewrite_metadata(
    path: impl AsRef<Path>,
    patch: &MetadataPatch,
) -> Result<SaveOutcome, SaveError> {
    let mut checkpoint = NoFault;
    rewrite_metadata_with(path.as_ref(), patch, &mut checkpoint)
}

pub fn rewrite_metadata_versioned(
    path: impl AsRef<Path>,
    expected_version: &FileVersion,
    patch: &MetadataPatch,
) -> Result<SaveCommit, SaveError> {
    let _operation = stillus_platform::OperationLock::file(path.as_ref())
        .map_err(|error| SaveError::InvalidTarget(error.to_string()))?;
    if patch.is_empty() {
        return Err(SaveError::InvalidTarget(
            "versioned metadata rewrite requires a non-empty patch".to_owned(),
        ));
    }
    let path = path.as_ref();
    let (mut input, opened_version) = open_versioned(path)?;
    if opened_version != *expected_version {
        return Err(version_conflict(
            "MetadataOpened",
            &opened_version,
            expected_version,
        ));
    }
    let scan = scan_reader(&mut input).map_err(|error| precommit(SaveStage::Scan, error))?;
    let body_offset = match scan.status {
        FrontMatterStatus::Plain => 0,
        FrontMatterStatus::Parsed(parsed) => parsed.body_offset,
        FrontMatterStatus::Invalid { issue, .. } => {
            return Err(SaveError::Patch(PatchError::InvalidFrontMatter(issue)));
        }
    };
    input
        .seek(SeekFrom::Start(body_offset))
        .map_err(|error| precommit(SaveStage::Scan, error))?;
    rewrite_note(path, expected_version, patch, move |writer| {
        copy_bounded(&mut input, writer).map(|_| ())
    })
}

pub fn open_versioned(path: impl AsRef<Path>) -> Result<(File, FileVersion), SaveError> {
    let path = path.as_ref();
    let target_metadata =
        fs::symlink_metadata(path).map_err(|error| precommit(SaveStage::OpenTarget, error))?;
    if !target_metadata.file_type().is_file() {
        return Err(SaveError::InvalidTarget(
            "target must be a regular file and not a symlink".to_owned(),
        ));
    }
    let version = FileVersion::from_metadata(&target_metadata);
    let file = File::open(path).map_err(|error| precommit(SaveStage::OpenTarget, error))?;
    let opened_version = file
        .metadata()
        .map(|metadata| FileVersion::from_metadata(&metadata))
        .map_err(|error| precommit(SaveStage::OpenTarget, error))?;
    if opened_version != version {
        return Err(version_conflict("OpenVersioned", &opened_version, &version));
    }
    Ok((file, version))
}

pub fn rewrite_note(
    path: impl AsRef<Path>,
    expected_version: &FileVersion,
    patch: &MetadataPatch,
    write_body: impl FnOnce(&mut File) -> io::Result<()>,
) -> Result<SaveCommit, SaveError> {
    let _operation = stillus_platform::OperationLock::file(path.as_ref())
        .map_err(|error| SaveError::InvalidTarget(error.to_string()))?;
    let mut checkpoint = NoFault;
    rewrite_note_with(
        path.as_ref(),
        expected_version,
        patch,
        write_body,
        &mut checkpoint,
    )
}

/// Atomically writes a body snapshot and, when `title` maps to another
/// portable filename, publishes that same snapshot at a collision-free path
/// before removing the verified source.
pub fn rewrite_note_with_title(
    workspace: impl AsRef<Path>,
    path: impl AsRef<Path>,
    expected_version: &FileVersion,
    patch: &MetadataPatch,
    title: &str,
    write_body: impl FnOnce(&mut File) -> io::Result<()>,
) -> Result<SaveCommit, SaveError> {
    let _operation = stillus_platform::OperationLock::directory(workspace.as_ref())
        .map_err(|error| SaveError::InvalidTarget(error.to_string()))?;
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (workspace, path, expected_version, patch, title, write_body);
        Err(SaveError::UnsupportedPlatform)
    }
    #[cfg(any(unix, windows))]
    {
        let workspace = workspace.as_ref();
        let path = path.as_ref();
        let notes = direct_notes_directory(workspace)
            .map_err(|error| SaveError::InvalidTarget(error.to_string()))?;
        validate_direct_note(path, &notes)
            .map_err(|error| SaveError::InvalidTarget(error.to_string()))?;
        let destination = available_title_path(&notes, title, Some(path))
            .map_err(|error| SaveError::InvalidTarget(error.to_string()))?;
        rewrite_note_to_destination(path, &destination, expected_version, patch, write_body)
    }
}

/// Atomically replaces a regular external file with the provided full-file
/// snapshot. Unlike note rewrites this does not scan, patch or synthesize
/// front matter and never changes the target path.
pub fn rewrite_external_file_versioned(
    path: impl AsRef<Path>,
    expected_version: &FileVersion,
    write_contents: impl FnOnce(&mut File) -> io::Result<()>,
) -> Result<SaveCommit, SaveError> {
    let _operation = stillus_platform::OperationLock::file(path.as_ref())
        .map_err(|error| SaveError::InvalidTarget(error.to_string()))?;
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, expected_version, write_contents);
        Err(SaveError::UnsupportedPlatform)
    }
    #[cfg(any(unix, windows))]
    {
        rewrite_external_file_versioned_unix(path.as_ref(), expected_version, write_contents)
    }
}

#[cfg(any(unix, windows))]
fn rewrite_external_file_versioned_unix(
    path: &Path,
    expected_version: &FileVersion,
    write_contents: impl FnOnce(&mut File) -> io::Result<()>,
) -> Result<SaveCommit, SaveError> {
    let target_metadata =
        fs::symlink_metadata(path).map_err(|error| precommit(SaveStage::OpenTarget, error))?;
    if !target_metadata.file_type().is_file()
        || FileVersion::from_metadata(&target_metadata) != *expected_version
    {
        return Err(SaveError::Conflict);
    }
    let opened = File::open(path).map_err(|error| precommit(SaveStage::OpenTarget, error))?;
    if FileVersion::from_metadata(
        &opened
            .metadata()
            .map_err(|error| precommit(SaveStage::OpenTarget, error))?,
    ) != *expected_version
    {
        return Err(SaveError::Conflict);
    }
    drop(opened);

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let (mut guard, mut temp) = create_temp(path, parent)?;
    temp.set_permissions(target_metadata.permissions())
        .map_err(|error| precommit(SaveStage::CreateTemp, error))?;
    write_contents(&mut temp).map_err(|error| precommit(SaveStage::Write, error))?;
    temp.flush()
        .map_err(|error| precommit(SaveStage::Write, error))?;
    temp.sync_all()
        .map_err(|error| precommit(SaveStage::FileSync, error))?;
    let temp_version = temp
        .metadata()
        .map(|metadata| FileVersion::from_metadata(&metadata))
        .map_err(|error| precommit(SaveStage::FileSync, error))?;

    let current =
        fs::symlink_metadata(path).map_err(|error| precommit(SaveStage::ConflictCheck, error))?;
    if !current.file_type().is_file() || FileVersion::from_metadata(&current) != *expected_version {
        return Err(SaveError::Conflict);
    }
    drop(temp);
    fs::rename(guard.path(), path).map_err(|error| precommit(SaveStage::Replace, error))?;
    guard.disarm();
    let committed = fs::symlink_metadata(path).map_err(|error| SaveError::PostReplaceSync {
        message: error.to_string(),
    })?;
    let committed_version = FileVersion::from_metadata(&committed);
    if !committed_version.same_file_as(&temp_version) {
        return Err(SaveError::PostReplaceSync {
            message: "external target changed immediately after atomic replace".to_owned(),
        });
    }
    stillus_platform::sync_directory(parent).map_err(|error| SaveError::PostReplaceSync {
        message: error.to_string(),
    })?;
    Ok(SaveCommit {
        outcome: SaveOutcome::Committed,
        version: committed_version,
        path: path.to_path_buf(),
    })
}

#[cfg(any(unix, windows))]
fn rewrite_note_to_destination(
    source: &Path,
    destination: &Path,
    expected_version: &FileVersion,
    patch: &MetadataPatch,
    write_body: impl FnOnce(&mut File) -> io::Result<()>,
) -> Result<SaveCommit, SaveError> {
    rewrite_note_to_destination_internal(
        source,
        destination,
        expected_version,
        patch,
        write_body,
        false,
    )
    .map(|(commit, _)| commit)
}

#[cfg(any(unix, windows))]
fn rewrite_note_to_destination_internal(
    source: &Path,
    destination: &Path,
    expected_version: &FileVersion,
    patch: &MetadataPatch,
    write_body: impl FnOnce(&mut File) -> io::Result<()>,
    hash_output: bool,
) -> Result<(SaveCommit, Option<String>), SaveError> {
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
    let scan = scan_reader(&mut input).map_err(|error| precommit(SaveStage::Scan, error))?;
    drop(input);
    let rewrite = patch_front_matter(&scan, patch)
        .map_err(SaveError::Patch)?
        .ok_or_else(|| {
            SaveError::InvalidTarget("note rewrite requires a non-empty metadata patch".to_owned())
        })?;
    let parent = source.parent().unwrap_or_else(|| Path::new("."));
    let (mut guard, mut temp) = create_temp(destination, parent)?;
    temp.set_permissions(source_metadata.permissions())
        .map_err(|error| precommit(SaveStage::CreateTemp, error))?;
    temp.write_all(&rewrite.prefix)
        .map_err(|error| precommit(SaveStage::Write, error))?;
    write_body(&mut temp).map_err(|error| precommit(SaveStage::Write, error))?;
    temp.flush()
        .map_err(|error| precommit(SaveStage::Write, error))?;
    temp.sync_all()
        .map_err(|error| precommit(SaveStage::FileSync, error))?;
    // Release the exclusive Windows writer before verification reopens the
    // completed ciphertext. TempGuard continues to own cleanup on any error.
    drop(temp);
    let expected_sha256 = hash_output
        .then(|| secure_backups::hash_file(guard.path()))
        .transpose()
        .map_err(|error| precommit(SaveStage::FileSync, error))?;
    let current =
        fs::symlink_metadata(source).map_err(|error| precommit(SaveStage::ConflictCheck, error))?;
    if !current.file_type().is_file() || FileVersion::from_metadata(&current) != *expected_version {
        return Err(SaveError::Conflict);
    }

    let destination_aliases_source = destination != source
        && fs::symlink_metadata(destination)
            .ok()
            .is_some_and(|metadata| {
                FileVersion::from_metadata(&metadata).same_file_as(expected_version)
            });
    if destination == source || destination_aliases_source {
        fs::rename(guard.path(), source).map_err(|error| precommit(SaveStage::Replace, error))?;
        guard.disarm();
        sync_directory_io(parent).map_err(|error| SaveError::PostReplaceSync {
            message: error.to_string(),
        })?;
        if destination_aliases_source {
            fs::rename(source, destination).map_err(|error| SaveError::PartialCommit {
                path: source.to_path_buf(),
                message: format!("case-only relocation failed: {error}"),
            })?;
            sync_directory_io(parent).map_err(|error| SaveError::PartialCommit {
                path: destination.to_path_buf(),
                message: error.to_string(),
            })?;
        }
        let committed_path = if destination_aliases_source {
            destination
        } else {
            source
        };
        let metadata =
            fs::symlink_metadata(committed_path).map_err(|error| SaveError::PostReplaceSync {
                message: error.to_string(),
            })?;
        return Ok((
            SaveCommit {
                outcome: SaveOutcome::Committed,
                version: FileVersion::from_metadata(&metadata),
                path: committed_path.to_path_buf(),
            },
            expected_sha256,
        ));
    }

    publish_temp(&mut guard, destination).map_err(|error| match error {
        NoteOperationError::Collision(_) => SaveError::Conflict,
        error => SaveError::PreCommit {
            stage: SaveStage::Replace,
            message: error.to_string(),
        },
    })?;
    if let Err(error) = sync_directory_io(parent) {
        let _ = fs::remove_file(destination);
        let _ = sync_directory_io(parent);
        return Err(precommit(SaveStage::ParentSync, error));
    }
    let before_remove =
        fs::symlink_metadata(source).map_err(|error| precommit(SaveStage::SourceRemove, error))?;
    if !before_remove.file_type().is_file()
        || FileVersion::from_metadata(&before_remove) != *expected_version
    {
        let _ = fs::remove_file(destination);
        let _ = sync_directory_io(parent);
        return Err(SaveError::Conflict);
    }
    if let Err(error) = fs::remove_file(source) {
        let rollback = fs::remove_file(destination);
        let _ = sync_directory_io(parent);
        return match rollback {
            Ok(()) => Err(precommit(SaveStage::SourceRemove, error)),
            Err(rollback_error) => Err(SaveError::PartialCommit {
                path: destination.to_path_buf(),
                message: format!(
                    "source removal failed ({error}); destination rollback failed ({rollback_error})"
                ),
            }),
        };
    }
    sync_directory_io(parent).map_err(|error| SaveError::PartialCommit {
        path: destination.to_path_buf(),
        message: error.to_string(),
    })?;
    let metadata = fs::symlink_metadata(destination).map_err(|error| SaveError::PartialCommit {
        path: destination.to_path_buf(),
        message: error.to_string(),
    })?;
    Ok((
        SaveCommit {
            outcome: SaveOutcome::Committed,
            version: FileVersion::from_metadata(&metadata),
            path: destination.to_path_buf(),
        },
        expected_sha256,
    ))
}

#[cfg(not(any(unix, windows)))]
fn rewrite_note_with(
    _path: &Path,
    _expected_version: &FileVersion,
    _patch: &MetadataPatch,
    _write_body: impl FnOnce(&mut File) -> io::Result<()>,
    _checkpoint: &mut impl Checkpoint,
) -> Result<SaveCommit, SaveError> {
    Err(SaveError::UnsupportedPlatform)
}

#[cfg(any(unix, windows))]
fn rewrite_note_with(
    path: &Path,
    expected_version: &FileVersion,
    patch: &MetadataPatch,
    write_body: impl FnOnce(&mut File) -> io::Result<()>,
    checkpoint: &mut impl Checkpoint,
) -> Result<SaveCommit, SaveError> {
    let target_metadata =
        fs::symlink_metadata(path).map_err(|error| precommit(SaveStage::OpenTarget, error))?;
    if !target_metadata.file_type().is_file() {
        return Err(SaveError::InvalidTarget(
            "target must be a regular file and not a symlink".to_owned(),
        ));
    }
    if FileVersion::from_metadata(&target_metadata) != *expected_version {
        return Err(version_conflict(
            "RewriteTarget",
            &FileVersion::from_metadata(&target_metadata),
            expected_version,
        ));
    }
    let mut input = File::open(path).map_err(|error| precommit(SaveStage::OpenTarget, error))?;
    let opened_version = input
        .metadata()
        .map(|metadata| FileVersion::from_metadata(&metadata))
        .map_err(|error| precommit(SaveStage::OpenTarget, error))?;
    if opened_version != *expected_version {
        return Err(version_conflict(
            "RewriteOpened",
            &opened_version,
            expected_version,
        ));
    }
    let scan = scan_reader(&mut input).map_err(|error| precommit(SaveStage::Scan, error))?;
    drop(input);
    let rewrite = patch_front_matter(&scan, patch)
        .map_err(SaveError::Patch)?
        .ok_or_else(|| {
            SaveError::InvalidTarget("note rewrite requires a non-empty metadata patch".to_owned())
        })?;

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let (mut guard, mut temp) = create_temp(path, parent)?;
    temp.set_permissions(target_metadata.permissions())
        .map_err(|error| precommit(SaveStage::CreateTemp, error))?;
    temp.write_all(&rewrite.prefix)
        .map_err(|error| precommit(SaveStage::Write, error))?;
    write_body(&mut temp).map_err(|error| precommit(SaveStage::Write, error))?;
    temp.flush()
        .map_err(|error| precommit(SaveStage::Write, error))?;
    checkpoint
        .check(SaveStage::Write)
        .map_err(|error| precommit(SaveStage::Write, error))?;
    checkpoint
        .check(SaveStage::FileSync)
        .map_err(|error| precommit(SaveStage::FileSync, error))?;
    temp.sync_all()
        .map_err(|error| precommit(SaveStage::FileSync, error))?;
    let temp_metadata = temp
        .metadata()
        .map_err(|error| precommit(SaveStage::FileSync, error))?;
    let temp_version = FileVersion::from_metadata(&temp_metadata);

    checkpoint
        .check(SaveStage::ConflictCheck)
        .map_err(|error| precommit(SaveStage::ConflictCheck, error))?;
    let current_metadata =
        fs::symlink_metadata(path).map_err(|error| precommit(SaveStage::ConflictCheck, error))?;
    if !current_metadata.file_type().is_file()
        || FileVersion::from_metadata(&current_metadata) != *expected_version
    {
        return Err(version_conflict(
            "RewriteBeforeReplace",
            &FileVersion::from_metadata(&current_metadata),
            expected_version,
        ));
    }

    checkpoint
        .check(SaveStage::Replace)
        .map_err(|error| precommit(SaveStage::Replace, error))?;
    drop(temp);
    #[cfg(windows)]
    replace_retry::replace_note(&mut guard, path, expected_version, &temp_metadata)?;
    #[cfg(unix)]
    {
        fs::rename(guard.path(), path).map_err(|error| precommit(SaveStage::Replace, error))?;
        guard.disarm();
    }
    let committed_metadata = fs::symlink_metadata(path)
        .map_err(|error| post_replace_failure("CommittedInspect", Some(error)))?;
    let committed_version = FileVersion::from_metadata(&committed_metadata);
    if !committed_version.same_file_as(&temp_version) {
        return Err(post_replace_failure("CommittedIdentity", None));
    }

    checkpoint
        .check(SaveStage::ParentSync)
        .map_err(|error| post_replace_failure("ParentCheckpoint", Some(error)))?;
    stillus_platform::sync_directory(parent)
        .map_err(|error| post_replace_failure("ParentSync", Some(error)))?;
    Ok(SaveCommit {
        outcome: SaveOutcome::Committed,
        version: committed_version,
        path: path.to_path_buf(),
    })
}

#[cfg(any(unix, windows))]
fn post_replace_failure(_stage: &'static str, error: Option<io::Error>) -> SaveError {
    #[cfg(any(test, feature = "test-utils"))]
    {
        let kind = match error.as_ref().map(io::Error::kind) {
            None => "IdentityMismatch",
            Some(io::ErrorKind::NotFound) => "NotFound",
            Some(io::ErrorKind::PermissionDenied) => "PermissionDenied",
            Some(io::ErrorKind::AlreadyExists) => "AlreadyExists",
            Some(io::ErrorKind::InvalidInput) => "InvalidInput",
            Some(io::ErrorKind::InvalidData) => "InvalidData",
            Some(io::ErrorKind::Unsupported) => "Unsupported",
            Some(io::ErrorKind::WouldBlock) => "WouldBlock",
            Some(io::ErrorKind::Interrupted) => "Interrupted",
            Some(io::ErrorKind::UnexpectedEof) => "UnexpectedEof",
            _ => "Other",
        };
        eprintln!(
            "NATIVE_POST_REPLACE thread={:?} stage={_stage} kind={kind} os_error={}",
            std::thread::current().id(),
            error
                .as_ref()
                .and_then(io::Error::raw_os_error)
                .unwrap_or(0),
        );
    }
    SaveError::PostReplaceSync {
        message: error.map_or_else(
            || "target changed immediately after atomic replace".to_owned(),
            |error| error.to_string(),
        ),
    }
}

#[cfg(not(any(unix, windows)))]
fn rewrite_metadata_with(
    _path: &Path,
    patch: &MetadataPatch,
    _checkpoint: &mut impl Checkpoint,
) -> Result<SaveOutcome, SaveError> {
    if patch.is_empty() {
        Ok(SaveOutcome::Unchanged)
    } else {
        Err(SaveError::UnsupportedPlatform)
    }
}

#[cfg(any(unix, windows))]
fn rewrite_metadata_with(
    path: &Path,
    patch: &MetadataPatch,
    checkpoint: &mut impl Checkpoint,
) -> Result<SaveOutcome, SaveError> {
    if patch.is_empty() {
        return Ok(SaveOutcome::Unchanged);
    }
    let target_metadata =
        fs::symlink_metadata(path).map_err(|error| precommit(SaveStage::OpenTarget, error))?;
    if !target_metadata.file_type().is_file() {
        return Err(SaveError::InvalidTarget(
            "target must be a regular file and not a symlink".to_owned(),
        ));
    }
    let original_identity = FileVersion::from_metadata(&target_metadata);
    let mut input = File::open(path).map_err(|error| precommit(SaveStage::OpenTarget, error))?;
    let opened_identity = input
        .metadata()
        .map(|metadata| FileVersion::from_metadata(&metadata))
        .map_err(|error| precommit(SaveStage::OpenTarget, error))?;
    if opened_identity != original_identity {
        return Err(SaveError::Conflict);
    }
    let scan = scan_reader(&mut input).map_err(|error| precommit(SaveStage::Scan, error))?;
    let Some(rewrite) = patch_front_matter(&scan, patch).map_err(SaveError::Patch)? else {
        return Ok(SaveOutcome::Unchanged);
    };

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let (mut guard, mut temp) = create_temp(path, parent)?;
    temp.set_permissions(target_metadata.permissions())
        .map_err(|error| precommit(SaveStage::CreateTemp, error))?;
    input
        .seek(SeekFrom::Start(rewrite.body_offset))
        .map_err(|error| precommit(SaveStage::Scan, error))?;
    temp.write_all(&rewrite.prefix)
        .map_err(|error| precommit(SaveStage::Write, error))?;
    copy_bounded(&mut input, &mut temp).map_err(|error| precommit(SaveStage::Write, error))?;
    drop(input);
    temp.flush()
        .map_err(|error| precommit(SaveStage::Write, error))?;
    checkpoint
        .check(SaveStage::Write)
        .map_err(|error| precommit(SaveStage::Write, error))?;
    checkpoint
        .check(SaveStage::FileSync)
        .map_err(|error| precommit(SaveStage::FileSync, error))?;
    temp.sync_all()
        .map_err(|error| precommit(SaveStage::FileSync, error))?;

    checkpoint
        .check(SaveStage::ConflictCheck)
        .map_err(|error| precommit(SaveStage::ConflictCheck, error))?;
    let current_metadata =
        fs::symlink_metadata(path).map_err(|error| precommit(SaveStage::ConflictCheck, error))?;
    if !current_metadata.file_type().is_file()
        || FileVersion::from_metadata(&current_metadata) != original_identity
    {
        return Err(SaveError::Conflict);
    }

    checkpoint
        .check(SaveStage::Replace)
        .map_err(|error| precommit(SaveStage::Replace, error))?;
    drop(temp);
    fs::rename(guard.path(), path).map_err(|error| precommit(SaveStage::Replace, error))?;
    guard.disarm();

    checkpoint
        .check(SaveStage::ParentSync)
        .map_err(|error| SaveError::PostReplaceSync {
            message: error.to_string(),
        })?;
    stillus_platform::sync_directory(parent).map_err(|error| SaveError::PostReplaceSync {
        message: error.to_string(),
    })?;
    Ok(SaveOutcome::Committed)
}

fn copy_bounded(reader: &mut impl Read, writer: &mut (impl Write + ?Sized)) -> io::Result<u64> {
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    let mut copied = 0_u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(copied);
        }
        writer.write_all(&buffer[..read])?;
        copied += read as u64;
    }
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

fn create_body_envelope_writer<W: Write>(
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

fn allocate_opaque_destination(parent: &Path) -> io::Result<PathBuf> {
    for _ in 0..32 {
        let name = opaque_note_filename().map_err(|_| {
            io::Error::other("could not generate an opaque protected-note filename")
        })?;
        let destination = parent.join(name);
        match fs::symlink_metadata(&destination) {
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(destination),
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate an opaque protected-note filename",
    ))
}

#[cfg(any(unix, windows))]
// Bind the guard before its writer: reverse local drop order closes every
// writer (including envelope wrappers) before cleanup, also on early errors.
fn create_secure_temp(parent: &Path) -> io::Result<(TempGuard, File)> {
    for _ in 0..32 {
        let opaque = opaque_note_filename()
            .map_err(|_| io::Error::other("could not generate protected-note temp filename"))?;
        let id = opaque
            .strip_prefix("ntrm-")
            .and_then(|name| name.strip_suffix(".md"))
            .ok_or_else(|| io::Error::other("invalid opaque filename"))?;
        let temp_path = parent.join(format!(".ntrm-secure-{id}.tmp"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        match options.open(&temp_path) {
            Ok(file) => return Ok((TempGuard::new(temp_path), file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate protected-note temp file",
    ))
}

fn sync_directory_io(path: &Path) -> io::Result<()> {
    stillus_platform::sync_directory(path)
}

#[cfg(any(unix, windows))]
fn create_temp(path: &Path, parent: &Path) -> Result<(TempGuard, File), SaveError> {
    let file_name = path
        .file_name()
        .ok_or_else(|| SaveError::InvalidTarget("target path has no file name".to_owned()))?;
    for _ in 0..32 {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let mut name = OsString::from(".");
        name.push(file_name);
        name.push(format!(".stillus-tmp-{}-{id}", std::process::id()));
        let temp_path = parent.join(name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        match options.open(&temp_path) {
            Ok(file) => return Ok((TempGuard::new(temp_path), file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(precommit(SaveStage::CreateTemp, error)),
        }
    }
    Err(SaveError::PreCommit {
        stage: SaveStage::CreateTemp,
        message: "could not allocate a unique same-directory temp name".to_owned(),
    })
}

fn precommit(stage: SaveStage, error: impl std::fmt::Display) -> SaveError {
    #[cfg(any(test, feature = "test-utils"))]
    eprintln!("NATIVE_SAVE stage={stage:?} outcome=PreCommit");
    SaveError::PreCommit {
        stage,
        message: error.to_string(),
    }
}

trait Checkpoint {
    fn check(&mut self, stage: SaveStage) -> io::Result<()>;
}

struct NoFault;

impl Checkpoint for NoFault {
    fn check(&mut self, _stage: SaveStage) -> io::Result<()> {
        Ok(())
    }
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
            let result = stillus_platform::diagnostics::io_result(
                stillus_platform::diagnostics::Operation::Cleanup,
                stillus_platform::diagnostics::Stage::Remove,
                fs::remove_file(&self.path),
            );
            #[cfg(any(test, feature = "test-utils"))]
            {
                let outcome = match &result {
                    Ok(()) => "Removed",
                    Err(error) if error.kind() == io::ErrorKind::NotFound => "Absent",
                    Err(_) => "Failed",
                };
                eprintln!("NATIVE_CLEANUP outcome={outcome}");
            }
            // Drop cannot replace the operation's original error.
            let _ = result;
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileVersion {
    size: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(any(unix, windows))]
    device: u64,
    #[cfg(any(unix, windows))]
    inode: u64,
    #[cfg(any(unix, windows))]
    changed_seconds: i64,
    #[cfg(any(unix, windows))]
    changed_nanoseconds: i64,
    #[cfg(windows)]
    digest: [u8; 32],
}

fn version_conflict(
    _site: &'static str,
    _actual: &FileVersion,
    _expected: &FileVersion,
) -> SaveError {
    #[cfg(any(test, feature = "test-utils"))]
    {
        #[cfg(any(unix, windows))]
        let identity = _actual.same_file_as(_expected).to_string();
        #[cfg(not(any(unix, windows)))]
        let identity = "Unavailable";
        #[cfg(any(unix, windows))]
        let changed = (_actual.changed_seconds == _expected.changed_seconds
            && _actual.changed_nanoseconds == _expected.changed_nanoseconds)
            .to_string();
        #[cfg(not(any(unix, windows)))]
        let changed = "Unavailable";
        #[cfg(windows)]
        let digest = (_actual.digest == _expected.digest).to_string();
        #[cfg(not(windows))]
        let digest = "Unavailable";
        eprintln!(
            "NATIVE_VERSION site={_site} identity_equal={identity} size_equal={} modified_equal={} changed_equal={changed} digest_equal={digest}",
            _actual.size == _expected.size,
            _actual.modified == _expected.modified,
        );
    }
    SaveError::Conflict
}

impl FileVersion {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            #[cfg(windows)]
            digest: metadata.digest(),
            size: metadata.len(),
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

    #[cfg(any(unix, windows))]
    fn same_file_as(&self, other: &Self) -> bool {
        self.device == other.device && self.inode == other.inode
    }

    #[cfg(any(unix, windows))]
    fn same_content_as(&self, other: &Self) -> bool {
        #[cfg(windows)]
        if self.digest != other.digest {
            return false;
        }
        self.same_file_as(other) && self.size == other.size && self.modified == other.modified
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::{Seek, SeekFrom, Write};
    use std::sync::atomic::{AtomicU64, Ordering};
    use stillus_frontmatter::{FrontMatterIssue, MAX_FRONT_MATTER_BYTES};

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    fn unique_test_path(label: &str) -> PathBuf {
        let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "stillus-storage-{label}-{}-{id}",
            std::process::id()
        ))
    }

    #[test]
    fn concurrent_external_saves_cannot_both_publish_the_same_version() {
        let root = unique_test_path("concurrent-save");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("external.txt");
        fs::write(&path, b"original").unwrap();
        let version = open_versioned(&path).unwrap().1;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let workers = [b"first".as_slice(), b"second".as_slice()]
            .into_iter()
            .map(|body| {
                let barrier = barrier.clone();
                let path = path.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    rewrite_external_file_versioned(path, &version, |writer| writer.write_all(body))
                })
            })
            .collect::<Vec<_>>();
        let results = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        for result in &results {
            let outcome = match result {
                Ok(_) => "Success",
                Err(SaveError::Conflict) => "Conflict",
                Err(SaveError::PreCommit { .. }) => "PreCommit",
                Err(SaveError::PostReplaceSync { .. }) => "PostReplaceSync",
                Err(SaveError::PartialCommit { .. }) => "PartialCommit",
                Err(SaveError::InvalidTarget(_)) => "InvalidTarget",
                Err(SaveError::Patch(_)) => "Patch",
                Err(SaveError::UnsupportedPlatform) => "UnsupportedPlatform",
            };
            eprintln!("NATIVE_RESULT operation=ExternalSave outcome={outcome}");
        }
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(SaveError::Conflict)))
                .count(),
            1
        );
        let bytes = fs::read(&path).unwrap();
        assert!(bytes == b"first" || bytes == b"second");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_initialization_creates_only_the_root_and_notes_directory() {
        let root = unique_test_path("initialize-new");
        assert!(!root.exists());

        initialize_workspace(&root).unwrap();

        assert!(root.is_dir());
        assert!(root.join("notes").is_dir());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_initialization_preserves_existing_entries_and_is_idempotent() {
        let root = unique_test_path("initialize-existing");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("keep.bin"), b"keep me").unwrap();

        initialize_workspace(&root).unwrap();
        initialize_workspace(&root).unwrap();

        assert_eq!(fs::read(root.join("keep.bin")).unwrap(), b"keep me");
        assert!(root.join("notes").is_dir());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_initialization_rejects_a_file_in_place_of_notes() {
        let root = unique_test_path("initialize-file");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("notes"), b"not a directory").unwrap();

        let error = initialize_workspace(&root).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read(root.join("notes")).unwrap(), b"not a directory");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn workspace_initialization_rejects_a_symlink_in_place_of_notes() {
        use std::os::unix::fs::symlink;

        let root = unique_test_path("initialize-symlink");
        let outside = unique_test_path("initialize-symlink-outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        symlink(&outside, root.join("notes")).unwrap();

        let error = initialize_workspace(&root).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(
            fs::symlink_metadata(root.join("notes"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    fn verified(save: VerifiedSave) -> SaveCommit {
        match save {
            VerifiedSave::Verified(commit) => commit,
            VerifiedSave::IntegrityFailure(failure) => {
                panic!("unexpected integrity failure: {}", failure.message)
            }
        }
    }

    struct TestWorkspace {
        root: PathBuf,
    }

    impl TestWorkspace {
        fn new() -> Self {
            let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir()
                .join(format!("stillus-storage-test-{}-{id}", std::process::id()));
            fs::create_dir_all(root.join("notes")).unwrap();
            Self { root }
        }

        fn note(&self, name: &str, content: &[u8]) -> PathBuf {
            let path = self.root.join("notes").join(name);
            fs::write(&path, content).unwrap();
            path
        }
    }

    impl Drop for TestWorkspace {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).unwrap();
        }
    }

    #[test]
    fn body_title_projection_strips_markdown_and_honors_scan_bounds() {
        assert_eq!(
            project_body_title(
                "\r\n  \r\n# Привет  *мир* [ссылка](https://example.test) `код`\r\n"
            ),
            Some("Привет  мир ссылка код".to_owned())
        );
        assert_eq!(project_body_title("\n***\n# Later"), None);
        assert_eq!(
            project_body_title(&format!("{}# Too late", "\n".repeat(BODY_TITLE_SCAN_LINES))),
            None
        );
        assert_eq!(
            project_body_title(&"a".repeat(BODY_TITLE_SCAN_BYTES + 1)),
            None
        );
    }

    #[test]
    fn scan_recognizes_crlf_armor_without_rewriting_it() {
        let workspace = TestWorkspace::new();
        let source = workspace.note(
            "Portable.md",
            b"---\ntitle: Portable\n---\nprivate\r\nbody\n",
        );
        let password = MasterPassword::new("portable note".to_owned());
        let version = open_versioned(&source).unwrap().1;
        protect_note_body(&source, &version, &password, "Portable").unwrap();
        let NoteScanResult::Protected(scan) = scan_note(&source).unwrap() else {
            panic!("expected protected note");
        };
        let bytes = fs::read(&source).unwrap();
        let armor = String::from_utf8(bytes[scan.body_offset as usize..].to_vec())
            .unwrap()
            .replace('\n', "\r\n");
        let mut crlf = bytes[..scan.body_offset as usize].to_vec();
        crlf.extend_from_slice(armor.as_bytes());
        fs::write(&source, &crlf).unwrap();
        let NoteScanResult::Protected(scan) = scan_note(&source).unwrap() else {
            panic!("expected CRLF protected note");
        };
        assert_eq!(fs::read(&source).unwrap(), crlf);
        assert_eq!(&crlf[scan.body_offset as usize..], armor.as_bytes());
        let mut file = File::open(&source).unwrap();
        file.seek(SeekFrom::Start(scan.body_offset)).unwrap();
        let mut reader = decrypt_body(file, &password).unwrap();
        let mut body = Vec::new();
        reader.read_to_end(&mut body).unwrap();
        assert_eq!(body, b"private\r\nbody\n");
    }

    #[test]
    fn legacy_protected_notes_are_rejected_without_rewriting_ciphertext() {
        let workspace = TestWorkspace::new();
        let password = MasterPassword::new("legacy password".to_owned());
        let body = b"legacy private body";
        let mut envelope =
            BodyEnvelopeWriter::new_for_test(Vec::new(), &password, body.len() as u64).unwrap();
        envelope.write_all(body).unwrap();
        let mut bytes = b"---\ntitle: Legacy\nlegacy_encryption: age-body-v1\n---\n".to_vec();
        bytes.extend_from_slice(&envelope.finish().unwrap());
        let source = workspace.note("Legacy.md", &bytes);
        assert!(matches!(
            scan_note(&source).unwrap(),
            NoteScanResult::InvalidProtected(_)
        ));
        let version = open_versioned(&source).unwrap().1;
        assert!(
            rewrite_protected_metadata_versioned(
                &workspace.root,
                &source,
                &version,
                &MetadataPatch {
                    pinned: Some(true),
                    ..MetadataPatch::default()
                },
                None,
            )
            .is_err()
        );
        assert_eq!(fs::read(source).unwrap(), bytes);
        assert!(!workspace.root.join(".stillus_backups").exists());
    }

    #[test]
    fn body_only_protection_keeps_metadata_and_locked_edits_copy_ciphertext_exactly() {
        let workspace = TestWorkspace::new();
        let source = workspace.note(
            "Visible Title.md",
            b"---\ntitle: Visible Title\ntags: [Work]\n# keep\nfuture_field: 42\n---\nVisible Title\nprivate-body-marker\n",
        );
        let password = MasterPassword::new("body-only password".to_owned());
        let version = open_versioned(&source).unwrap().1;
        let protected = protect_note_body(&source, &version, &password, "Visible Title").unwrap();
        assert_eq!(protected.path, source);
        let bytes = fs::read(&source).unwrap();
        assert!(contains_bytes(&bytes, b"title: 'Visible Title'"));
        assert!(contains_bytes(&bytes, b"tags: [Work]"));
        assert!(contains_bytes(&bytes, b"future_field: 42"));
        assert!(contains_bytes(&bytes, b"stillus_encryption: age-body-v1"));
        assert!(!contains_bytes(&bytes, b"private-body-marker"));

        let NoteScanResult::Protected(scan) = scan_note(&source).unwrap() else {
            panic!("expected body-only protected note");
        };
        assert_eq!(scan.version, protected.version);
        let original_armor = bytes[scan.body_offset as usize..].to_vec();
        let edited = verified(
            rewrite_protected_metadata_versioned(
                &workspace.root,
                &source,
                &protected.version,
                &MetadataPatch {
                    tags: Some(vec!["Work".to_owned(), "Visible".to_owned()]),
                    pinned: Some(true),
                    order: Some(BTreeMap::from([("Work".to_owned(), 3)])),
                    ..MetadataPatch::default()
                },
                None,
            )
            .unwrap(),
        );
        let edited_bytes = fs::read(&source).unwrap();
        let NoteScanResult::Protected(edited_scan) = scan_note(&source).unwrap() else {
            panic!("expected protected note after metadata edit");
        };
        assert_eq!(
            &edited_bytes[edited_scan.body_offset as usize..],
            original_armor.as_slice()
        );
        let FrontMatterStatus::Parsed(edited_frontmatter) = &edited_scan.frontmatter.status else {
            panic!("expected parsed protected front matter after metadata edit");
        };
        assert_eq!(edited_frontmatter.metadata.order.get("Work"), Some(&3));

        let before_wrong_password = edited_bytes.clone();
        assert!(
            disable_body_protection(
                &workspace.root,
                &source,
                &edited.version,
                &MasterPassword::new("wrong".to_owned()),
                "Visible Title",
            )
            .is_err()
        );
        assert_eq!(fs::read(&source).unwrap(), before_wrong_password);

        let disabled = verified(
            disable_body_protection(
                &workspace.root,
                &source,
                &edited.version,
                &password,
                "Visible Title",
            )
            .unwrap(),
        );
        assert_eq!(disabled.path, source);
        let plaintext = fs::read_to_string(&source).unwrap();
        assert!(!plaintext.contains("stillus_encryption"));
        assert!(plaintext.contains("future_field: 42"));
        assert!(plaintext.ends_with("Visible Title\nprivate-body-marker\n"));
    }

    #[test]
    fn protected_autosave_uses_title_collision_path_and_scanner_fails_closed() {
        let workspace = TestWorkspace::new();
        let source = workspace.note("Old.md", b"Old\nsecret-before\n");
        workspace.note("New.md", b"collision must survive");
        let password = MasterPassword::new("collision password".to_owned());
        let version = open_versioned(&source).unwrap().1;
        let protected = protect_note_body(&source, &version, &password, "Old").unwrap();
        let body = b"New\nsecret-after\n";
        let committed = verified(
            rewrite_protected_body_with_title(
                &workspace.root,
                &source,
                &protected.version,
                ProtectedBodyRewrite {
                    password: &password,
                    patch: &MetadataPatch {
                        title: Some("New".to_owned()),
                        ..MetadataPatch::default()
                    },
                    title: "New",
                    body_len: body.len() as u64,
                },
                |writer| writer.write_all(body),
            )
            .unwrap(),
        );
        assert_eq!(committed.path, workspace.root.join("notes/New (2).md"));
        assert!(!source.exists());
        assert_eq!(
            fs::read(workspace.root.join("notes/New.md")).unwrap(),
            b"collision must survive"
        );
        let NoteScanResult::Protected(scan) = scan_note(&committed.path).unwrap() else {
            panic!("expected protected collision destination");
        };
        let mut input = File::open(&committed.path).unwrap();
        input.seek(SeekFrom::Start(scan.body_offset)).unwrap();
        let mut reader = decrypt_body(input, &password).unwrap();
        let mut decrypted = Vec::new();
        reader.read_to_end(&mut decrypted).unwrap();
        assert_eq!(decrypted, body);

        let marker_without_armor = workspace.note(
            "Broken marker.md",
            b"---\ntitle: Broken\nstillus_encryption: age-body-v1\n---\nplaintext\n",
        );
        assert!(matches!(
            scan_note(marker_without_armor).unwrap(),
            NoteScanResult::InvalidProtected(_)
        ));

        let mut armor =
            BodyEnvelopeWriter::new_for_test(Vec::new(), &password, b"hidden".len() as u64)
                .unwrap();
        armor.write_all(b"hidden").unwrap();
        let armor = armor.finish().unwrap();
        let mut missing_marker = b"---\ntitle: Broken\n---\n".to_vec();
        missing_marker.extend_from_slice(&armor);
        let missing_marker = workspace.note("Broken armor.md", &missing_marker);
        assert!(matches!(
            scan_note(missing_marker).unwrap(),
            NoteScanResult::InvalidProtected(_)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn protected_integrity_failure_keeps_exact_private_backup_and_restores_it() {
        let workspace = TestWorkspace::new();
        let source = workspace.note("Secret.md", b"Secret\nold-private-body\n");
        let password = MasterPassword::new("manifest-must-not-contain-this".to_owned());
        let version = open_versioned(&source).unwrap().1;
        let protected = protect_note_body(&source, &version, &password, "Secret").unwrap();
        let original = fs::read(&source).unwrap();
        fs::create_dir_all(workspace.root.join(".stillus")).unwrap();
        fs::write(
            workspace.root.join(".stillus/test-corrupt-protected-save"),
            b"once",
        )
        .unwrap();
        let body = b"Secret\nnew-private-body\n";
        let failure = match rewrite_protected_body_with_title(
            &workspace.root,
            &source,
            &protected.version,
            ProtectedBodyRewrite {
                password: &password,
                patch: &MetadataPatch {
                    title: Some("Secret".to_owned()),
                    ..MetadataPatch::default()
                },
                title: "Secret",
                body_len: body.len() as u64,
            },
            |writer| writer.write_all(body),
        )
        .unwrap()
        {
            VerifiedSave::IntegrityFailure(failure) => failure,
            VerifiedSave::Verified(_) => panic!("fault injector did not corrupt the save"),
        };
        assert_eq!(fs::read(&failure.backup.path).unwrap(), original);
        let NoteScanResult::Protected(backup_scan) = scan_note(&failure.backup.path).unwrap()
        else {
            panic!("rollback backup is not a standard protected note");
        };
        let mut backup_input = File::open(&failure.backup.path).unwrap();
        backup_input
            .seek(SeekFrom::Start(backup_scan.body_offset))
            .unwrap();
        let mut backup_body = String::new();
        decrypt_body(backup_input, &password)
            .unwrap()
            .read_to_string(&mut backup_body)
            .unwrap();
        assert!(backup_body.contains("old-private-body"));
        assert_ne!(fs::read(&source).unwrap(), original);
        assert_eq!(
            load_pending_integrity_failure(&workspace.root).unwrap(),
            Some((*failure).clone())
        );

        let manifest =
            fs::read_to_string(workspace.root.join(".stillus_backups/secure/manifest.json"))
                .unwrap();
        assert!(!manifest.contains("old-private-body"));
        assert!(!manifest.contains("new-private-body"));
        assert!(!manifest.contains("manifest-must-not-contain-this"));
        assert_eq!(
            fs::metadata(workspace.root.join(".stillus_backups"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&failure.backup.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let restored = restore_secure_backup(&workspace.root, &failure).unwrap();
        assert_eq!(restored.path, source);
        assert_eq!(fs::read(&source).unwrap(), original);
        assert!(
            load_pending_integrity_failure(&workspace.root)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn protected_restore_rejects_external_candidate_change_and_keeps_backup() {
        let workspace = TestWorkspace::new();
        let source = workspace.note("Conflict.md", b"Conflict\nold body\n");
        let password = MasterPassword::new("conflict password".to_owned());
        let version = open_versioned(&source).unwrap().1;
        let protected = protect_note_body(&source, &version, &password, "Conflict").unwrap();
        fs::create_dir_all(workspace.root.join(".stillus")).unwrap();
        fs::write(
            workspace.root.join(".stillus/test-corrupt-protected-save"),
            b"once",
        )
        .unwrap();
        let body = b"Conflict\nnew body\n";
        let failure = match rewrite_protected_body_with_title(
            &workspace.root,
            &source,
            &protected.version,
            ProtectedBodyRewrite {
                password: &password,
                patch: &MetadataPatch {
                    modified: Some("2026-09-04T00:00:00Z".to_owned()),
                    ..MetadataPatch::default()
                },
                title: "Conflict",
                body_len: body.len() as u64,
            },
            |writer| writer.write_all(body),
        )
        .unwrap()
        {
            VerifiedSave::IntegrityFailure(failure) => failure,
            VerifiedSave::Verified(_) => panic!("fault injector did not corrupt the save"),
        };
        OpenOptions::new()
            .append(true)
            .open(&failure.commit.path)
            .unwrap()
            .write_all(b"external")
            .unwrap();
        assert!(matches!(
            restore_secure_backup(&workspace.root, &failure),
            Err(SaveError::Conflict)
        ));
        assert!(failure.backup.path.is_file());
        assert!(matches!(
            load_pending_integrity_failure(&workspace.root),
            Err(SaveError::Conflict)
        ));
        let manifest: serde_json::Value = serde_json::from_slice(
            &fs::read(workspace.root.join(".stillus_backups/secure/manifest.json")).unwrap(),
        )
        .unwrap();
        assert!(!manifest["notes"][0]["pending"].is_null());
    }

    #[test]
    fn protected_post_commit_read_failure_is_reported_with_rollback_backup() {
        let workspace = TestWorkspace::new();
        let source = workspace.note("Unreadable.md", b"Unreadable\nold body\n");
        let password = MasterPassword::new("read failure password".to_owned());
        let version = open_versioned(&source).unwrap().1;
        let protected = protect_note_body(&source, &version, &password, "Unreadable").unwrap();
        let original = fs::read(&source).unwrap();
        fs::create_dir_all(workspace.root.join(".stillus")).unwrap();
        fs::write(
            workspace.root.join(".stillus/test-corrupt-protected-save"),
            b"remove",
        )
        .unwrap();
        let body = b"Unreadable\nnew body\n";
        let failure = match rewrite_protected_body_with_title(
            &workspace.root,
            &source,
            &protected.version,
            ProtectedBodyRewrite {
                password: &password,
                patch: &MetadataPatch {
                    modified: Some("2026-09-04T00:00:00Z".to_owned()),
                    ..MetadataPatch::default()
                },
                title: "Unreadable",
                body_len: body.len() as u64,
            },
            |writer| writer.write_all(body),
        )
        .unwrap()
        {
            VerifiedSave::IntegrityFailure(failure) => failure,
            VerifiedSave::Verified(_) => panic!("read fault did not fail verification"),
        };
        assert!(failure.actual_sha256.is_none());
        assert!(failure.message.contains("could not be verified"));
        assert_eq!(fs::read(failure.backup.path).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn protected_backup_retention_is_ten_and_unknown_symlink_store_is_rejected() {
        use std::os::unix::fs::symlink;

        let workspace = TestWorkspace::new();
        let source = workspace.note("Rotate.md", b"Rotate\nbody\n");
        let password = MasterPassword::new("rotation password".to_owned());
        let version = open_versioned(&source).unwrap().1;
        let mut commit = protect_note_body(&source, &version, &password, "Rotate").unwrap();
        for index in 0..11 {
            commit = verified(
                rewrite_protected_metadata_versioned(
                    &workspace.root,
                    &source,
                    &commit.version,
                    &MetadataPatch {
                        pinned: Some(index % 2 == 0),
                        modified: Some(format!("2026-09-04T00:00:{index:02}Z")),
                        ..MetadataPatch::default()
                    },
                    None,
                )
                .unwrap(),
            );
        }
        let manifest: serde_json::Value = serde_json::from_slice(
            &fs::read(workspace.root.join(".stillus_backups/secure/manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            manifest["notes"][0]["backups"].as_array().unwrap().len(),
            10
        );
        let backup_directory = workspace
            .root
            .join(".stillus_backups/secure")
            .join(manifest["notes"][0]["id"].as_str().unwrap());
        assert_eq!(fs::read_dir(backup_directory).unwrap().count(), 10);

        let unsafe_workspace = TestWorkspace::new();
        let unsafe_source = unsafe_workspace.note("Unsafe.md", b"Unsafe\nbody\n");
        let unsafe_password = MasterPassword::new("rejected-path password".to_owned());
        let unsafe_version = open_versioned(&unsafe_source).unwrap().1;
        let unsafe_protected =
            protect_note_body(&unsafe_source, &unsafe_version, &unsafe_password, "Unsafe").unwrap();
        let outside = unsafe_workspace.root.join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, unsafe_workspace.root.join(".stillus_backups")).unwrap();
        let before = fs::read(&unsafe_source).unwrap();
        assert!(
            rewrite_protected_metadata_versioned(
                &unsafe_workspace.root,
                &unsafe_source,
                &unsafe_protected.version,
                &MetadataPatch {
                    pinned: Some(true),
                    ..MetadataPatch::default()
                },
                None,
            )
            .is_err()
        );
        assert_eq!(fs::read(&unsafe_source).unwrap(), before);
        assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
    }

    #[test]
    fn portable_filename_projection_is_bounded_and_collision_ready() {
        assert_eq!(safe_note_filename_stem(" Road/Map:* ", 1), "Road∕Map");
        assert_eq!(safe_note_filename_stem("CON", 1), EMPTY_NOTE_TITLE);
        assert_eq!(safe_note_filename_stem("Title", 2), "Title (2)");
        let long = safe_note_filename_stem(&"Я".repeat(200), 27);
        assert!(long.chars().count() <= MAX_NOTE_TITLE_CHARS);
        assert!(long.len() <= MAX_NOTE_TITLE_BYTES);
        assert!(long.ends_with(" (27)"));
    }

    #[test]
    fn title_driven_save_relocates_once_without_overwriting_collision() {
        let workspace = TestWorkspace::new();
        let source = workspace.note("Old.md", b"---\ntitle: Old\nmodified: old\n---\n# Old\n");
        workspace.note("New ∕ Name.md", b"collision");
        let (_, version) = open_versioned(&source).unwrap();
        let body = b"# New / Name\nbody\n";
        let commit = rewrite_note_with_title(
            &workspace.root,
            &source,
            &version,
            &MetadataPatch {
                title: Some("New / Name".to_owned()),
                modified: Some("now".to_owned()),
                ..MetadataPatch::default()
            },
            "New / Name",
            |writer| writer.write_all(body),
        )
        .unwrap();
        assert_eq!(commit.path.file_name().unwrap(), "New ∕ Name (2).md");
        assert!(!source.exists());
        let saved = fs::read(&commit.path).unwrap();
        assert!(saved.ends_with(body));
        assert_eq!(
            fs::read(workspace.root.join("notes/New ∕ Name.md")).unwrap(),
            b"collision"
        );
    }

    #[test]
    fn portable_note_names_and_tags_are_validated_conservatively() {
        for accepted in ["Project Alpha", "  Привет мир  ", "O'Brien", "A.B"] {
            let validated = validate_note_title(accepted).unwrap();
            assert_eq!(validated, accepted.trim());
        }
        for rejected in [
            "",
            " ",
            ".",
            "..",
            ".hidden",
            "bad/child",
            "bad\\child",
            "bad:name",
            "bad.",
            "CON",
            "com1.txt",
            "LPT9",
            "bad\nname",
        ] {
            assert!(
                matches!(
                    validate_note_title(rejected),
                    Err(NoteOperationError::InvalidName(_))
                ),
                "unexpected accepted name: {rejected:?}"
            );
        }
        assert!(validate_note_title(&"x".repeat(MAX_NOTE_TITLE_BYTES + 1)).is_err());

        for accepted in ["Work", "  Задачи  ", "Clients/Alpha", "A:B"] {
            assert_eq!(validate_tag(accepted).unwrap(), accepted.trim());
        }
        for rejected in ["", " ", "bad\ntag"] {
            assert!(matches!(
                validate_tag(rejected),
                Err(NoteOperationError::InvalidTag(_))
            ));
        }
        assert!(validate_tag(&"x".repeat(MAX_TAG_BYTES + 1)).is_err());
    }

    #[test]
    fn generated_portable_names_cannot_escape_the_notes_directory() {
        const ALPHABET: &[char] = &[
            'a', 'Z', '0', ' ', '.', '_', '-', '/', '\\', '<', '>', ':', '"', '|', '?', '*', '\n',
            '\0', 'é', 'Ж', '🦀',
        ];
        let mut random = Lcg::new(0x5041_5448_0000_0008);
        let notes = Path::new("workspace/notes");
        for _ in 0..2_048 {
            let length = random.next_usize(48);
            let value = (0..length)
                .map(|_| ALPHABET[random.next_usize(ALPHABET.len())])
                .collect::<String>();
            if let Ok(validated) = validate_note_title(&value) {
                assert_eq!(validated, validated.trim());
                assert!(!validated.is_empty());
                assert!(!validated.starts_with('.'));
                assert!(!validated.ends_with([' ', '.']));
                assert!(
                    !validated.chars().any(
                        |character| character.is_control() || "/\\<>:\"|?*".contains(character)
                    )
                );
                let path = notes.join(format!("{validated}.md"));
                assert_eq!(path.parent(), Some(notes));
                assert_eq!(
                    path.extension().and_then(|value| value.to_str()),
                    Some("md")
                );
            }
            if let Ok(validated) = validate_tag(&value) {
                assert_eq!(validated, validated.trim());
                assert!(!validated.is_empty());
                assert!(!validated.chars().any(char::is_control));
            }
        }
    }

    #[test]
    fn create_is_notable_compatible_and_never_overwrites_case_collision() {
        let workspace = TestWorkspace::new();
        let timestamp = "2026-09-01T12:34:56.789Z";
        let commit = create_note(&workspace.root, "  O'Brien  ", timestamp).unwrap();
        assert_eq!(commit.path.file_name().unwrap(), "O'Brien.md");
        let output = fs::read_to_string(&commit.path).unwrap();
        assert!(output.contains("favorited: false\n"));
        assert!(output.contains("pinned: false\n"));
        assert!(output.contains("tags: []\n"));
        assert!(output.contains("title: 'O''Brien'\n"));
        assert!(output.contains(&format!("created: '{timestamp}'\n")));
        assert!(output.contains(&format!("modified: '{timestamp}'\n")));
        assert!(output.ends_with("---\n\n# O'Brien"));
        let scan = scan_reader(output.as_bytes()).unwrap();
        let FrontMatterStatus::Parsed(parsed) = scan.status else {
            panic!("created note must have parsed front matter");
        };
        assert_eq!(parsed.metadata.title.as_deref(), Some("O'Brien"));
        assert_eq!(parsed.metadata.created.as_deref(), Some(timestamp));
        assert_eq!(parsed.metadata.modified.as_deref(), Some(timestamp));
        assert_eq!(open_versioned(&commit.path).unwrap().1, commit.version);

        let original = fs::read(&commit.path).unwrap();
        let collision = create_note(&workspace.root, "o'brien", timestamp).unwrap();
        assert_eq!(collision.path.file_name().unwrap(), "o'brien (2).md");
        assert_eq!(fs::read(&commit.path).unwrap(), original);
        assert_no_temp_files(&workspace);
    }

    #[cfg(unix)]
    #[test]
    fn rename_preserves_body_unknown_metadata_permissions_and_rolls_back_fault() {
        let workspace = TestWorkspace::new();
        let original = b"---\n# keep\ntitle: Old\ncreated: '2022-02-03T18:57:43.598Z'\nfuture: {value: 7}\n---\n# Body\nbytes\n";
        let source = workspace.note("old.md", original);
        fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).unwrap();
        let version = open_versioned(&source).unwrap().1;
        let commit = rename_note(
            &workspace.root,
            &source,
            &version,
            "Новое имя",
            "2026-09-01T12:34:56.789Z",
        )
        .unwrap();
        assert!(!source.exists());
        assert_eq!(commit.path.file_name().unwrap(), "Новое имя.md");
        let output = fs::read_to_string(&commit.path).unwrap();
        assert!(output.contains("# keep\n"));
        assert!(output.contains("future: {value: 7}\n"));
        assert!(output.contains("created: '2022-02-03T18:57:43.598Z'\n"));
        assert!(output.contains("title: 'Новое имя'\n"));
        assert!(output.ends_with("# Body\nbytes\n"));
        assert_eq!(
            fs::metadata(&commit.path).unwrap().permissions().mode() & 0o777,
            0o640
        );

        let fault_source = workspace.note("fault.md", original);
        let fault_version = open_versioned(&fault_source).unwrap().1;
        let mut fail = FailOperationAt {
            stage: OperationStage::SourceRemove,
        };
        let result = rename_note_with(
            &workspace.root,
            &fault_source,
            &fault_version,
            "Rolled Back",
            "2026-09-01T12:34:56.789Z",
            &mut fail,
        );
        assert!(matches!(
            result,
            Err(NoteOperationError::Failed {
                stage: OperationStage::SourceRemove,
                ..
            })
        ));
        assert_eq!(fs::read(&fault_source).unwrap(), original);
        assert!(!workspace.root.join("notes/Rolled Back.md").exists());
        assert_no_temp_files(&workspace);
    }

    #[test]
    fn rename_supports_case_only_destination_alias_without_weakening_collisions() {
        struct CaseInsensitiveRenameFilesystem;

        impl RenameFilesystem for CaseInsensitiveRenameFilesystem {
            fn destination_aliases_source(
                &self,
                source: &Path,
                destination: &Path,
            ) -> Result<bool, NoteOperationError> {
                Ok(source.file_name().zip(destination.file_name()).is_some_and(
                    |(source, destination)| {
                        source.to_string_lossy().to_lowercase()
                            == destination.to_string_lossy().to_lowercase()
                    },
                ))
            }

            fn rename(&self, source: &Path, destination: &Path) -> io::Result<()> {
                fs::rename(source, destination)
            }
        }

        let workspace = TestWorkspace::new();
        let original = b"---\ntitle: project\nfuture: keep\n---\nbody\n";
        let source = workspace.note("project.md", original);
        let version = open_versioned(&source).unwrap().1;
        let mut checkpoint = NoOperationFault;
        let commit = rename_note_with_filesystem(
            &workspace.root,
            &source,
            &version,
            "Project",
            "2026-09-02T12:34:56.789Z",
            &mut checkpoint,
            &CaseInsensitiveRenameFilesystem,
        )
        .unwrap();

        assert_eq!(commit.path.file_name().unwrap(), "Project.md");
        let names = fs::read_dir(workspace.root.join("notes"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(names, [OsString::from("Project.md")]);
        assert_eq!(open_versioned(&commit.path).unwrap().1, commit.version);
        let output = fs::read_to_string(&commit.path).unwrap();
        assert!(output.contains("title: 'Project'\n"));
        assert!(output.contains("modified: '2026-09-02T12:34:56.789Z'\n"));
        assert!(output.contains("future: keep\n"));
        assert!(output.ends_with("body\n"));
        assert_no_temp_files(&workspace);
    }

    #[test]
    fn rename_rejects_a_distinct_hardlink_destination() {
        let workspace = TestWorkspace::new();
        let original = b"---\ntitle: project\nfuture: keep\n---\nbody\n";
        let hardlink_source = workspace.note("hardlink.md", original);
        let hardlink_destination = workspace.root.join("notes/Other.md");
        fs::hard_link(&hardlink_source, &hardlink_destination).unwrap();
        let hardlink_version = open_versioned(&hardlink_source).unwrap().1;
        assert!(matches!(
            rename_note(
                &workspace.root,
                &hardlink_source,
                &hardlink_version,
                "Other",
                "2026-09-02T12:34:56.789Z"
            ),
            Err(NoteOperationError::Collision(path)) if path == hardlink_destination
        ));
        assert_eq!(fs::read(&hardlink_source).unwrap(), original);
        assert_eq!(fs::read(&hardlink_destination).unwrap(), original);
    }

    #[test]
    fn rename_collision_and_trash_are_recoverable_without_touching_other_files() {
        let workspace = TestWorkspace::new();
        let original = b"---\ntitle: Original\n---\nbody\n";
        let source = workspace.note("source.md", original);
        let occupied = workspace.note("Taken.md", b"occupied");
        let version = open_versioned(&source).unwrap().1;
        assert!(matches!(
            rename_note(
                &workspace.root,
                &source,
                &version,
                "taken",
                "2026-09-01T12:34:56.789Z"
            ),
            Err(NoteOperationError::Collision(_))
        ));
        assert_eq!(fs::read(&source).unwrap(), original);
        assert_eq!(fs::read(&occupied).unwrap(), b"occupied");

        let first = trash_note(&workspace.root, &source, &version).unwrap();
        assert!(!source.exists());
        assert_eq!(first.trash_path.file_name().unwrap(), "source.md");
        assert_eq!(fs::read(&first.trash_path).unwrap(), original);
        let source = workspace.note("source.md", original);
        let version = open_versioned(&source).unwrap().1;
        let second = trash_note(&workspace.root, &source, &version).unwrap();
        assert_eq!(second.trash_path.file_name().unwrap(), "source.1.md");
        assert_eq!(fs::read(&second.trash_path).unwrap(), original);
        assert_eq!(fs::read(&occupied).unwrap(), b"occupied");
    }

    #[cfg(unix)]
    #[test]
    fn trash_rejects_state_symlink_and_rolls_back_before_source_remove() {
        use std::os::unix::fs::symlink;

        let workspace = TestWorkspace::new();
        let original = b"---\ntitle: Keep\n---\nbody\n";
        let source = workspace.note("keep.md", original);
        let version = open_versioned(&source).unwrap().1;
        let outside = workspace.root.join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, workspace.root.join(".stillus")).unwrap();
        assert!(matches!(
            trash_note(&workspace.root, &source, &version),
            Err(NoteOperationError::InvalidWorkspace(_))
        ));
        assert_eq!(fs::read(&source).unwrap(), original);
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);

        fs::remove_file(workspace.root.join(".stillus")).unwrap();
        let mut fail = FailOperationAt {
            stage: OperationStage::SourceRemove,
        };
        let result = trash_note_with(&workspace.root, &source, &version, &mut fail);
        assert!(matches!(
            result,
            Err(NoteOperationError::Failed {
                stage: OperationStage::SourceRemove,
                ..
            })
        ));
        assert_eq!(fs::read(&source).unwrap(), original);
        assert_eq!(
            fs::read_dir(workspace.root.join(".stillus/trash"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn protected_trash_partial_commit_moves_only_ciphertext() {
        let workspace = TestWorkspace::new();
        let marker = b"protected-trash-plaintext-marker-0024";
        let source = workspace.note("Protected Trash.md", marker);
        let password = MasterPassword::new("protected trash password".to_owned());
        let version = open_versioned(&source).unwrap().1;
        let protected = protect_note(&workspace.root, &source, &version, &password).unwrap();
        let ciphertext = fs::read(&protected.path).unwrap();
        assert!(
            !ciphertext
                .windows(marker.len())
                .any(|window| window == marker)
        );

        let mut fail_notes_sync = FailOperationOccurrence {
            stage: OperationStage::DirectorySync,
            remaining: 2,
        };
        let result = trash_note_with(
            &workspace.root,
            &protected.path,
            &protected.version,
            &mut fail_notes_sync,
        );
        assert!(matches!(
            result,
            Err(NoteOperationError::PartialCommit { .. })
        ));
        assert!(!protected.path.exists());
        let trash_files = fs::read_dir(workspace.root.join(".stillus/trash"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(trash_files.len(), 1);
        assert_eq!(fs::read(&trash_files[0]).unwrap(), ciphertext);
        assert!(scan_workspace(&workspace.root).unwrap().notes.is_empty());
    }

    #[test]
    fn scans_direct_regular_markdown_files_in_path_order() {
        let workspace = TestWorkspace::new();
        workspace.note("b.md", b"# plain");
        workspace.note("a.md", b"---\ntitle: A\ntags: [One]\n---\nbody");
        workspace.note("ignored.txt", b"---\ntitle: ignored\n---");
        fs::create_dir(workspace.root.join("notes").join("nested")).unwrap();
        fs::write(
            workspace
                .root
                .join("notes")
                .join("nested")
                .join("hidden.md"),
            b"---\ntitle: hidden\n---",
        )
        .unwrap();

        let scan = scan_workspace(&workspace.root).unwrap();
        let names = scan
            .notes
            .iter()
            .map(|note| note.path.file_name().unwrap().to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, ["a.md", "b.md"]);
        assert_eq!(parsed_notes(&scan).count(), 1);
    }

    #[test]
    fn protection_is_streaming_opaque_and_restart_repairable() {
        use stillus_secure::{EnvelopeKind, MasterPassword, decrypt};

        let workspace = TestWorkspace::new();
        let marker = "storage-protected-body-marker-76c4d92a";
        let original =
            format!("---\ntitle: Private Marker 8e91\ntags: [SecretTag41]\n---\n{marker}\n");
        let source = workspace.note("Sensitive Filename 3a57.md", original.as_bytes());
        let version = open_versioned(&source).unwrap().1;
        let password = MasterPassword::new("storage-test-password".to_owned());
        let commit = protect_note(&workspace.root, &source, &version, &password).unwrap();

        assert!(!source.exists());
        let opaque_name = commit.path.file_name().unwrap().to_str().unwrap();
        assert!(is_opaque_note_name(opaque_name));
        let ciphertext = fs::read(&commit.path).unwrap();
        for secret in [
            marker.as_bytes(),
            b"Private Marker 8e91".as_slice(),
            b"SecretTag41".as_slice(),
            b"Sensitive Filename 3a57.md".as_slice(),
        ] {
            assert!(
                !ciphertext
                    .windows(secret.len())
                    .any(|window| window == secret)
            );
        }
        let scan = scan_workspace(&workspace.root).unwrap();
        assert!(matches!(
            scan.notes[0].result,
            NoteScanResult::LegacyProtected
        ));

        let mut reader = decrypt(
            File::open(&commit.path).unwrap(),
            &password,
            EnvelopeKind::Note,
        )
        .unwrap();
        assert_eq!(
            reader.metadata().original_filename,
            "Sensitive Filename 3a57.md"
        );
        let mut decrypted = Vec::new();
        reader.read_to_end(&mut decrypted).unwrap();
        assert_eq!(decrypted, original.as_bytes());

        assert!(
            fs::read_dir(workspace.root.join("notes"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".ntrm-transition-"))
        );
    }

    #[test]
    fn repair_workspace_leaves_legacy_transition_and_scan_is_read_only() {
        let workspace = TestWorkspace::new();
        let password = MasterPassword::new("journal repair password".to_owned());
        let source = workspace.note(
            "Interrupted Secret Name.md",
            b"---\ntitle: Journalled\n---\nprivate journal marker\n",
        );
        let version = open_versioned(&source).unwrap().1;
        let mut fail_after_publish = FailOperationAt {
            stage: OperationStage::DirectorySync,
        };
        let interrupted = protect_note_with(
            &workspace.root,
            &source,
            &version,
            &password,
            &mut fail_after_publish,
        );
        assert!(matches!(
            interrupted,
            Err(NoteOperationError::PartialCommit { .. })
        ));
        assert!(source.exists());
        assert!(path_has_age_prefix(&source).unwrap());
        assert!(
            fs::read_dir(workspace.root.join("notes"))
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_str()
                    .is_some_and(is_protection_journal_name))
        );

        let before_repair = scan_workspace(&workspace.root).unwrap();
        assert_eq!(before_repair.notes.len(), 1);
        assert_eq!(before_repair.notes[0].path, source);
        assert!(source.exists());
        assert!(
            fs::read_dir(workspace.root.join("notes"))
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_str()
                    .is_some_and(is_protection_journal_name))
        );

        repair_workspace(&workspace.root).unwrap();
        let repaired = scan_workspace(&workspace.root).unwrap();
        assert_eq!(repaired.notes.len(), 1);
        assert!(matches!(
            repaired.notes[0].result,
            NoteScanResult::InvalidProtected(_)
        ));
        assert_eq!(repaired.notes[0].path, source);
        assert!(source.exists());
        assert!(
            fs::read_dir(workspace.root.join("notes"))
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_str()
                    .is_some_and(is_protection_journal_name))
        );

        let foreign_path = workspace.note("Foreign age.md", b"placeholder");
        let foreign_file = File::create(&foreign_path).unwrap();
        let foreign_plaintext = b"foreign age payload";
        let metadata = EnvelopeMetadata::new(
            EnvelopeKind::Note,
            "foreign-original.md".to_owned(),
            foreign_plaintext.len() as u64,
        )
        .unwrap();
        let mut writer = EnvelopeWriter::new_for_test(foreign_file, &password, metadata).unwrap();
        writer.write_all(foreign_plaintext).unwrap();
        writer.finish().unwrap().sync_all().unwrap();
        let foreign_before = fs::read(&foreign_path).unwrap();
        let literal_path = workspace.note(
            "Literal prefix.md",
            b"age-encryption.org/v1\nthis is ordinary markdown text\n",
        );
        let literal_before = fs::read(&literal_path).unwrap();

        let scan = scan_workspace(&workspace.root).unwrap();
        let foreign = scan
            .notes
            .iter()
            .find(|note| note.path == foreign_path)
            .unwrap();
        let literal = scan
            .notes
            .iter()
            .find(|note| note.path == literal_path)
            .unwrap();
        assert!(matches!(
            foreign.result,
            NoteScanResult::InvalidProtected(_)
        ));
        assert!(matches!(literal.result, NoteScanResult::Scanned(_)));
        assert_eq!(fs::read(&foreign_path).unwrap(), foreign_before);
        assert_eq!(fs::read(&literal_path).unwrap(), literal_before);
    }

    #[test]
    fn concurrent_workspace_repair_is_an_idempotent_protection_commit() {
        let workspace = TestWorkspace::new();
        let original = b"---\ntitle: Concurrent repair\n---\nprotected race payload\n";
        let source = workspace.note("Concurrent repair.md", original);
        let version = open_versioned(&source).unwrap().1;
        let password = MasterPassword::new("concurrent-repair-password".to_owned());
        let mut repair = RepairProtectionAtPublish {
            workspace: &workspace.root,
            remaining: 2,
            repaired: false,
        };

        let commit =
            protect_note_with(&workspace.root, &source, &version, &password, &mut repair).unwrap();

        assert!(repair.repaired);
        assert!(!source.exists());
        assert!(is_opaque_note_name(
            commit.path.file_name().unwrap().to_str().unwrap()
        ));
        assert!(path_has_age_prefix(&commit.path).unwrap());
        let scan = scan_workspace(&workspace.root).unwrap();
        assert_eq!(scan.notes.len(), 1);
        assert_eq!(scan.notes[0].path, commit.path);
        assert!(matches!(
            scan.notes[0].result,
            NoteScanResult::LegacyProtected
        ));
        let mut decrypted = decrypt(
            File::open(&commit.path).unwrap(),
            &password,
            EnvelopeKind::Note,
        )
        .unwrap();
        let mut bytes = Vec::new();
        decrypted.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, original);
        assert!(
            fs::read_dir(workspace.root.join("notes"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_str()
                    .is_some_and(is_protection_journal_name))
        );
        assert_no_secure_temp_files(&workspace);
    }

    #[test]
    fn protected_rewrite_never_materializes_plaintext_temp() {
        use stillus_secure::{EnvelopeKind, MasterPassword, decrypt};

        let workspace = TestWorkspace::new();
        let source = workspace.note("Private.md", b"---\ntitle: Private\n---\nold marker\n");
        let version = open_versioned(&source).unwrap().1;
        let password = MasterPassword::new("rewrite-test-password".to_owned());
        let commit = protect_note(&workspace.root, &source, &version, &password).unwrap();
        let prefix = b"---\ntitle: Private\nmodified: '2026-09-02T00:00:00.000Z'\n---\n";
        let body = b"new protected marker 52ea\n";
        let rewrite = rewrite_protected_note(
            &commit.path,
            &commit.version,
            &password,
            "Private.md",
            prefix,
            body.len() as u64,
            |writer| writer.write_all(body),
        )
        .unwrap();
        assert_eq!(rewrite.outcome, SaveOutcome::Committed);
        let ciphertext = fs::read(&commit.path).unwrap();
        assert!(!ciphertext.windows(body.len()).any(|window| window == body));
        assert!(
            fs::read_dir(workspace.root.join("notes"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".ntrm-secure-"))
        );
        let mut reader = decrypt(
            File::open(&commit.path).unwrap(),
            &password,
            EnvelopeKind::Note,
        )
        .unwrap();
        let mut plaintext = Vec::new();
        reader.read_to_end(&mut plaintext).unwrap();
        assert_eq!(plaintext, [prefix.as_slice(), body.as_slice()].concat());
    }

    #[test]
    fn protection_conflict_preserves_current_plaintext_byte_for_byte() {
        let workspace = TestWorkspace::new();
        let source = workspace.note("Conflict.md", b"initial");
        let stale = open_versioned(&source).unwrap().1;
        fs::write(&source, b"external current bytes").unwrap();
        let current = fs::read(&source).unwrap();
        let result = protect_note(
            &workspace.root,
            &source,
            &stale,
            &MasterPassword::new("conflict-password".to_owned()),
        );
        assert!(matches!(result, Err(NoteOperationError::Conflict)));
        assert_eq!(fs::read(&source).unwrap(), current);
    }

    #[cfg(unix)]
    #[test]
    fn disable_protection_restores_original_bytes_name_and_permissions() {
        let workspace = TestWorkspace::new();
        let original = b"---\ntitle: Private\ntags: [Secret]\n---\nUnicode body: \xd0\x9f\xd1\x80\xd0\xb8\xd0\xb2\xd0\xb5\xd1\x82\n";
        let source = workspace.note("Private Unicode.md", original);
        fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).unwrap();
        let password = MasterPassword::new("disable-test-password".to_owned());
        let version = open_versioned(&source).unwrap().1;
        let protected = protect_note(&workspace.root, &source, &version, &password).unwrap();

        let restored = disable_protection(
            &workspace.root,
            &protected.path,
            &protected.version,
            &password,
        )
        .unwrap();

        assert_eq!(restored.path, source);
        assert_eq!(fs::read(&restored.path).unwrap(), original);
        assert_eq!(
            fs::metadata(&restored.path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(!protected.path.exists());
        assert_eq!(open_versioned(&restored.path).unwrap().1, restored.version);
        assert_no_secure_temp_files(&workspace);
    }

    #[test]
    fn protection_faults_preserve_plaintext_before_publish_and_ciphertext_after_it() {
        let cases = [
            (OperationStage::Validate, 1, false),
            (OperationStage::Write, 1, false),
            (OperationStage::Write, 2, false),
            (OperationStage::FileSync, 1, false),
            (OperationStage::FileSync, 2, false),
            (OperationStage::Publish, 1, false),
            (OperationStage::DirectorySync, 1, true),
            (OperationStage::Publish, 2, true),
            (OperationStage::DirectorySync, 2, true),
            (OperationStage::SourceRemove, 1, true),
            (OperationStage::DirectorySync, 3, true),
        ];
        for (stage, occurrence, committed) in cases {
            let workspace = TestWorkspace::new();
            let marker = b"protect-fault-plaintext-marker-0024";
            let source = workspace.note("Fault Boundary.md", marker);
            let version = open_versioned(&source).unwrap().1;
            let password = MasterPassword::new("fault boundary password".to_owned());
            let mut fault = FailOperationOccurrence {
                stage,
                remaining: occurrence,
            };
            let result =
                protect_note_with(&workspace.root, &source, &version, &password, &mut fault);

            if committed {
                assert!(matches!(
                    result,
                    Err(NoteOperationError::PartialCommit { .. })
                ));
                let mut encrypted_note_paths = Vec::new();
                for entry in fs::read_dir(workspace.root.join("notes")).unwrap() {
                    let path = entry.unwrap().path();
                    if path.is_file() {
                        assert!(
                            !fs::read(&path)
                                .unwrap()
                                .windows(marker.len())
                                .any(|window| window == marker)
                        );
                        if path.extension().is_some_and(|extension| extension == "md")
                            && path_has_age_prefix(&path).unwrap()
                        {
                            encrypted_note_paths.push(path);
                        }
                    }
                }
                assert!(!encrypted_note_paths.is_empty());
                let opaque_destination_was_published = matches!(
                    (stage, occurrence),
                    (OperationStage::DirectorySync, 2)
                        | (OperationStage::SourceRemove, 1)
                        | (OperationStage::DirectorySync, 3)
                );
                if opaque_destination_was_published {
                    assert!(encrypted_note_paths.iter().any(|path| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(is_opaque_note_name)
                    }));
                }
                repair_workspace(&workspace.root).unwrap();
                let scan = scan_workspace(&workspace.root).unwrap();
                assert!(scan.notes.iter().any(|note| matches!(
                    note.result,
                    NoteScanResult::LegacyProtected | NoteScanResult::InvalidProtected(_)
                )));
            } else {
                assert!(matches!(result, Err(NoteOperationError::Failed { .. })));
                assert_eq!(fs::read(&source).unwrap(), marker);
                assert!(
                    scan_workspace(&workspace.root)
                        .unwrap()
                        .notes
                        .iter()
                        .all(|note| !matches!(note.result, NoteScanResult::LegacyProtected))
                );
            }
            assert_no_secure_temp_files(&workspace);
        }
    }

    #[test]
    fn disable_wrong_password_tamper_and_collision_preserve_canonical_files() {
        let workspace = TestWorkspace::new();
        let original = b"---\ntitle: Private\n---\nsecret marker 81ce\n";
        let source = workspace.note("Private.md", original);
        let password = MasterPassword::new("correct-disable-password".to_owned());
        let version = open_versioned(&source).unwrap().1;
        let protected = protect_note(&workspace.root, &source, &version, &password).unwrap();
        let ciphertext = fs::read(&protected.path).unwrap();

        let wrong = disable_protection(
            &workspace.root,
            &protected.path,
            &protected.version,
            &MasterPassword::new("wrong-disable-password".to_owned()),
        );
        assert!(matches!(wrong, Err(NoteOperationError::Failed { .. })));
        assert_eq!(fs::read(&protected.path).unwrap(), ciphertext);
        assert!(!source.exists());
        assert_no_secure_temp_files(&workspace);

        fs::write(&source, b"occupied destination bytes").unwrap();
        let collision = disable_protection(
            &workspace.root,
            &protected.path,
            &protected.version,
            &password,
        );
        assert!(matches!(collision, Err(NoteOperationError::Collision(_))));
        assert_eq!(fs::read(&source).unwrap(), b"occupied destination bytes");
        assert_eq!(fs::read(&protected.path).unwrap(), ciphertext);
        assert_no_secure_temp_files(&workspace);
        fs::remove_file(&source).unwrap();

        let mut tampered = ciphertext;
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        fs::write(&protected.path, &tampered).unwrap();
        let tampered_version = open_versioned(&protected.path).unwrap().1;
        let rejected = disable_protection(
            &workspace.root,
            &protected.path,
            &tampered_version,
            &password,
        );
        assert!(matches!(rejected, Err(NoteOperationError::Failed { .. })));
        assert_eq!(fs::read(&protected.path).unwrap(), tampered);
        assert!(!source.exists());
        assert_no_secure_temp_files(&workspace);
    }

    #[test]
    fn disable_faults_clean_plaintext_temp_and_report_post_publish_partial_commit() {
        let workspace = TestWorkspace::new();
        let original = b"---\ntitle: Fault Private\n---\nplaintext fault marker 5bdd\n";
        let source = workspace.note("Fault Private.md", original);
        let password = MasterPassword::new("fault-disable-password".to_owned());
        let version = open_versioned(&source).unwrap().1;
        let protected = protect_note(&workspace.root, &source, &version, &password).unwrap();
        let ciphertext = fs::read(&protected.path).unwrap();

        let mut before_publish = FailOperationAt {
            stage: OperationStage::FileSync,
        };
        let failed = disable_protection_with(
            &workspace.root,
            &protected.path,
            &protected.version,
            &password,
            &mut before_publish,
        );
        assert!(matches!(
            failed,
            Err(NoteOperationError::Failed {
                stage: OperationStage::FileSync,
                ..
            })
        ));
        assert_eq!(fs::read(&protected.path).unwrap(), ciphertext);
        assert!(!source.exists());
        assert_no_secure_temp_files(&workspace);

        let mut after_publish = FailOperationAt {
            stage: OperationStage::SourceRemove,
        };
        let partial = disable_protection_with(
            &workspace.root,
            &protected.path,
            &protected.version,
            &password,
            &mut after_publish,
        );
        assert!(matches!(
            partial,
            Err(NoteOperationError::PartialCommit { .. })
        ));
        assert_eq!(fs::read(&protected.path).unwrap(), ciphertext);
        assert_eq!(fs::read(&source).unwrap(), original);
        assert_no_secure_temp_files(&workspace);
    }

    #[test]
    fn disable_never_removes_a_changed_encrypted_source_after_plaintext_publish() {
        let workspace = TestWorkspace::new();
        let original = b"---\ntitle: Race Private\n---\noriginal plaintext\n";
        let external = b"external ciphertext replacement";
        let source = workspace.note("Race Private.md", original);
        let password = MasterPassword::new("race-disable-password".to_owned());
        let version = open_versioned(&source).unwrap().1;
        let protected = protect_note(&workspace.root, &source, &version, &password).unwrap();
        let mut mutate = MutateOperationAt {
            stage: OperationStage::SourceRemove,
            path: protected.path.clone(),
            content: external,
        };

        let result = disable_protection_with(
            &workspace.root,
            &protected.path,
            &protected.version,
            &password,
            &mut mutate,
        );

        assert!(matches!(
            result,
            Err(NoteOperationError::PartialCommit { .. })
        ));
        assert_eq!(fs::read(&protected.path).unwrap(), external);
        assert_eq!(fs::read(&source).unwrap(), original);
        assert_no_secure_temp_files(&workspace);
    }

    #[cfg(unix)]
    #[test]
    fn startup_cleanup_requires_owned_journal_and_preserves_foreign_files() {
        use std::os::unix::fs::symlink;
        use std::time::{Duration, SystemTime};

        let workspace = TestWorkspace::new();
        let source = workspace.note("Cleanup Private.md", b"private bytes");
        let password = MasterPassword::new("cleanup-password".to_owned());
        let version = open_versioned(&source).unwrap().1;
        let protected = protect_note(&workspace.root, &source, &version, &password).unwrap();
        let notes = workspace.root.join("notes");
        let plaintext = notes.join(".ntrm-secure-00000000000000000000000000000001.tmp");
        let fresh = notes.join(".ntrm-secure-00000000000000000000000000000003.tmp");
        let malformed = notes.join(".ntrm-secure-00000000000000000000000000000004.tmp");
        let foreign_envelope = notes.join(".ntrm-secure-00000000000000000000000000000005.tmp");
        let decoy = notes.join(".ntrm-secure-not-owned.tmp");
        let outside = workspace.root.join("outside-marker");
        let symlinked = notes.join(".ntrm-secure-00000000000000000000000000000002.tmp");
        let hardlink_source = workspace.root.join("foreign-hardlink-source");
        let hardlinked = notes.join(".ntrm-secure-00000000000000000000000000000006.tmp");
        fs::write(&plaintext, b"stale plaintext").unwrap();
        fs::write(&fresh, b"fresh active plaintext").unwrap();
        fs::write(&malformed, [AGE_PREFIX, b"malformed\n"].concat()).unwrap();
        fs::write(&decoy, b"unrelated bytes").unwrap();
        fs::write(&outside, b"outside bytes").unwrap();
        symlink(&outside, &symlinked).unwrap();
        fs::write(&hardlink_source, b"foreign hardlinked bytes").unwrap();
        fs::hard_link(&hardlink_source, &hardlinked).unwrap();

        let foreign_payload = b"foreign encrypted bytes";
        let foreign_file = File::create(&foreign_envelope).unwrap();
        let foreign_metadata = EnvelopeMetadata::new(
            EnvelopeKind::Note,
            "Foreign.md".to_owned(),
            foreign_payload.len() as u64,
        )
        .unwrap();
        let mut foreign_writer =
            EnvelopeWriter::new_for_test(foreign_file, &password, foreign_metadata).unwrap();
        foreign_writer.write_all(foreign_payload).unwrap();
        foreign_writer.finish().unwrap().sync_all().unwrap();

        let old_times = fs::FileTimes::new()
            .set_modified(SystemTime::now() - Duration::from_secs(25 * 60 * 60));
        for path in [
            &plaintext,
            &malformed,
            &foreign_envelope,
            &hardlink_source,
            &decoy,
            &protected.path,
        ] {
            OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(old_times)
                .unwrap();
        }

        let owned_payload = b"owned encrypted temp";
        let (mut owned_guard, owned_file) = create_secure_temp(&notes).unwrap();
        let owned_metadata = EnvelopeMetadata::new(
            EnvelopeKind::Note,
            "Owned.md".to_owned(),
            owned_payload.len() as u64,
        )
        .unwrap();
        let mut owned_writer =
            EnvelopeWriter::new_for_test(owned_file, &password, owned_metadata).unwrap();
        owned_writer.write_all(owned_payload).unwrap();
        let owned_file = owned_writer.finish().unwrap();
        owned_file.sync_all().unwrap();
        owned_file.set_times(old_times).unwrap();
        let owned_file_metadata = owned_file.metadata().unwrap();
        let destination = notes.join("ntrm-00000000000000000000000000000007.md");
        let mut journal_guard = create_protection_journal(
            &notes,
            owned_guard.path(),
            &owned_file_metadata,
            &destination,
        )
        .unwrap();
        let owned_path = owned_guard.path().to_path_buf();
        let journal_path = journal_guard.path().to_path_buf();
        drop(owned_file);
        owned_guard.disarm();
        journal_guard.disarm();

        assert_eq!(cleanup_stale_secure_temps(&workspace.root).unwrap(), 0);
        assert_eq!(cleanup_stale_secure_temps(&workspace.root).unwrap(), 0);
        let scan = scan_workspace(&workspace.root).unwrap();

        assert_eq!(scan.notes.len(), 1);
        assert!(matches!(
            scan.notes[0].result,
            NoteScanResult::LegacyProtected
        ));
        assert!(protected.path.exists());
        assert!(owned_path.exists());
        assert!(journal_path.exists());
        assert_eq!(fs::read(&plaintext).unwrap(), b"stale plaintext");
        assert_eq!(fs::read(&fresh).unwrap(), b"fresh active plaintext");
        assert_eq!(
            fs::read(&malformed).unwrap(),
            [AGE_PREFIX, b"malformed\n"].concat()
        );
        assert!(foreign_envelope.exists());
        assert_eq!(fs::read(&decoy).unwrap(), b"unrelated bytes");
        assert!(
            fs::symlink_metadata(&symlinked)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&hardlinked).unwrap(), b"foreign hardlinked bytes");
        assert_eq!(
            fs::read(&hardlink_source).unwrap(),
            b"foreign hardlinked bytes"
        );
        assert_eq!(fs::read(&outside).unwrap(), b"outside bytes");
    }

    #[test]
    fn startup_cleanup_preserves_changed_or_hardlinked_journaled_temps() {
        use std::time::{Duration, SystemTime};

        for hardlink in [false, true] {
            let workspace = TestWorkspace::new();
            let notes = workspace.root.join("notes");
            let password = MasterPassword::new("cleanup-identity-password".to_owned());
            let payload = b"journaled encrypted temp";
            let (mut temp_guard, file) = create_secure_temp(&notes).unwrap();
            let envelope_metadata = EnvelopeMetadata::new(
                EnvelopeKind::Note,
                "Identity.md".to_owned(),
                payload.len() as u64,
            )
            .unwrap();
            let mut writer =
                EnvelopeWriter::new_for_test(file, &password, envelope_metadata).unwrap();
            writer.write_all(payload).unwrap();
            let file = writer.finish().unwrap();
            file.sync_all().unwrap();
            let old_times = fs::FileTimes::new()
                .set_modified(SystemTime::now() - Duration::from_secs(25 * 60 * 60));
            file.set_times(old_times).unwrap();
            let file_metadata = file.metadata().unwrap();
            let destination = notes.join("ntrm-11111111111111111111111111111111.md");
            let mut journal_guard =
                create_protection_journal(&notes, temp_guard.path(), &file_metadata, &destination)
                    .unwrap();
            let temp_path = temp_guard.path().to_path_buf();
            let journal_path = journal_guard.path().to_path_buf();
            drop(file);
            temp_guard.disarm();
            journal_guard.disarm();

            let linked_path = workspace.root.join("foreign-hardlink");
            if hardlink {
                fs::hard_link(&temp_path, &linked_path).unwrap();
            } else {
                fs::write(
                    &temp_path,
                    [AGE_PREFIX, b"malformed after journal\n"].concat(),
                )
                .unwrap();
            }
            let before = fs::read(&temp_path).unwrap();

            assert_eq!(cleanup_stale_secure_temps(&workspace.root).unwrap(), 0);
            assert_eq!(fs::read(&temp_path).unwrap(), before);
            assert!(journal_path.exists());
            if hardlink {
                assert_eq!(fs::read(&linked_path).unwrap(), before);
            }

            let _ = scan_workspace(&workspace.root).unwrap();
            assert_eq!(fs::read(&temp_path).unwrap(), before);
            assert!(journal_path.exists());
            if hardlink {
                assert_eq!(fs::read(&linked_path).unwrap(), before);
            }
        }
    }

    #[test]
    fn malformed_note_does_not_hide_other_notes() {
        let workspace = TestWorkspace::new();
        workspace.note("bad.md", b"---\ntags: not-an-array\n---\nbody");
        workspace.note("good.md", b"---\ntitle: Good\n---\nbody");

        let scan = scan_workspace(&workspace.root).unwrap();
        assert_eq!(scan.notes.len(), 2);
        assert_eq!(parsed_notes(&scan).count(), 1);
        assert!(matches!(
            &scan.notes[0].result,
            NoteScanResult::Scanned(ScannedNote {
                frontmatter: FrontMatterScan {
                    status: FrontMatterStatus::Invalid {
                        issue: FrontMatterIssue::MalformedYaml(_),
                        ..
                    },
                    ..
                },
                ..
            })
        ));
    }

    #[test]
    fn sparse_one_gigabyte_body_keeps_scan_bounded() {
        let workspace = TestWorkspace::new();
        let path = workspace.root.join("notes").join("large.md");
        let mut file = File::create(&path).unwrap();
        file.write_all(b"---\ntitle: Large\ntags: [Scale]\n---\n")
            .unwrap();
        file.seek(SeekFrom::Start(999_999_999)).unwrap();
        file.write_all(b"\n").unwrap();
        file.sync_all().unwrap();
        drop(file);
        assert_eq!(fs::metadata(&path).unwrap().len(), 1_000_000_000);

        let scan = scan_workspace(&workspace.root).unwrap();
        let NoteScanResult::Scanned(result) = &scan.notes[0].result else {
            panic!("expected a scan result");
        };
        assert!(matches!(
            result.frontmatter.status,
            FrontMatterStatus::Parsed(_)
        ));
        assert!(result.frontmatter.bytes_read <= 1_024);
        assert!(result.frontmatter.bytes_read < MAX_FRONT_MATTER_BYTES);
        assert_eq!(fs::metadata(&path).unwrap().len(), 1_000_000_000);
    }

    #[cfg(unix)]
    #[test]
    fn does_not_follow_symlinked_notes() {
        use std::os::unix::fs::symlink;

        let workspace = TestWorkspace::new();
        let outside = workspace.root.join("outside.md");
        fs::write(&outside, b"---\ntitle: Outside\n---\n").unwrap();
        symlink(&outside, workspace.root.join("notes").join("linked.md")).unwrap();

        let scan = scan_workspace(&workspace.root).unwrap();
        assert!(scan.notes.is_empty());
        assert!(matches!(
            scan_note(workspace.root.join("notes/linked.md")),
            Err(error) if error.kind() == io::ErrorKind::InvalidInput
        ));
    }

    #[cfg(unix)]
    #[test]
    fn atomic_rewrite_preserves_unknown_body_permissions_and_unrelated_files() {
        let workspace = TestWorkspace::new();
        let input = b"---\n# preserved\ntitle: Old\ncreated: '2022-02-03T18:57:43.598Z'\nfuture: {value: 123}\n---\n\n# Body\nbytes\n";
        let path = workspace.note("note.md", input);
        let unrelated = workspace.note("unrelated.md", b"untouched");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        let outcome = rewrite_metadata(
            &path,
            &MetadataPatch {
                title: Some("New".to_owned()),
                tags: Some(vec!["Work".to_owned(), "Задачи".to_owned()]),
                modified: Some("2026-09-01T00:00:00.000Z".to_owned()),
                ..MetadataPatch::default()
            },
        )
        .unwrap();
        assert_eq!(outcome, SaveOutcome::Committed);

        let output = fs::read(&path).unwrap();
        let output_text = std::str::from_utf8(&output).unwrap();
        assert!(output_text.contains("# preserved\n"));
        assert!(output_text.contains("future: {value: 123}\n"));
        assert!(output_text.contains("created: '2022-02-03T18:57:43.598Z'\n"));
        assert!(output_text.contains("title: 'New'\n"));
        assert!(output_text.ends_with("---\n\n# Body\nbytes\n"));
        assert_eq!(fs::read(&unrelated).unwrap(), b"untouched");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_no_temp_files(&workspace);
    }

    #[cfg(unix)]
    #[test]
    fn external_rewrite_is_whole_file_versioned_and_preserves_permissions() {
        let workspace = TestWorkspace::new();
        let path = workspace.root.join("External.txt");
        let original = b"---\ntitle: literal yaml\n---\nbody\n";
        fs::write(&path, original).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let (_, version) = open_versioned(&path).unwrap();

        let replacement = b"---\nnot: metadata\n---\nchanged\n";
        let commit =
            rewrite_external_file_versioned(&path, &version, |file| file.write_all(replacement))
                .unwrap();
        assert_eq!(commit.path, path);
        assert_eq!(fs::read(&path).unwrap(), replacement);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );

        assert!(matches!(
            rewrite_external_file_versioned(&path, &version, |file| file.write_all(b"stale")),
            Err(SaveError::Conflict)
        ));
        assert_eq!(fs::read(&path).unwrap(), replacement);
    }

    #[test]
    fn versioned_note_rewrite_streams_new_body_and_preserves_metadata() {
        let workspace = TestWorkspace::new();
        let path = workspace.note(
            "note.md",
            b"---\ntitle: Keep\ncreated: '2022-02-03T18:57:43.598Z'\nfuture: yes\n---\nold body\n",
        );
        #[cfg(windows)]
        let permissions = fs::metadata(&path).unwrap().permissions();
        let (_, version) = open_versioned(&path).unwrap();
        let body = "new 🦀 body\n";
        let commit = rewrite_note(
            &path,
            &version,
            &MetadataPatch {
                modified: Some("2026-09-01T12:34:56.789Z".to_owned()),
                ..MetadataPatch::default()
            },
            |writer| writer.write_all(body.as_bytes()),
        )
        .unwrap();

        assert_eq!(commit.outcome, SaveOutcome::Committed);
        let output = fs::read_to_string(&path).unwrap();
        assert!(output.contains("title: Keep\n"));
        assert!(output.contains("created: '2022-02-03T18:57:43.598Z'\n"));
        assert!(output.contains("future: yes\n"));
        assert!(output.contains("modified: '2026-09-01T12:34:56.789Z'\n"));
        assert!(output.ends_with(body));
        let (_, current) = open_versioned(&path).unwrap();
        assert_eq!(current, commit.version);
        #[cfg(windows)]
        assert_eq!(fs::metadata(&path).unwrap().permissions(), permissions);
        assert_no_temp_files(&workspace);
    }

    #[test]
    fn note_rewrite_rejects_stale_version_and_body_writer_failure() {
        let workspace = TestWorkspace::new();
        let original = b"---\ntitle: Original\n---\nbody\n";
        let path = workspace.note("note.md", original);
        let (_, version) = open_versioned(&path).unwrap();

        let error = rewrite_note(
            &path,
            &version,
            &MetadataPatch {
                modified: Some("2026-09-01T00:00:00.000Z".to_owned()),
                ..MetadataPatch::default()
            },
            |_writer| Err(io::Error::other("injected body failure")),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            SaveError::PreCommit {
                stage: SaveStage::Write,
                ..
            }
        ));
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_no_temp_files(&workspace);

        fs::write(&path, b"---\ntitle: External\n---\nexternal\n").unwrap();
        let error = rewrite_note(
            &path,
            &version,
            &MetadataPatch {
                modified: Some("2026-09-01T00:00:01.000Z".to_owned()),
                ..MetadataPatch::default()
            },
            |writer| writer.write_all(b"local\n"),
        )
        .unwrap_err();
        assert_eq!(error, SaveError::Conflict);
        assert_eq!(
            fs::read(&path).unwrap(),
            b"---\ntitle: External\n---\nexternal\n"
        );
        assert_no_temp_files(&workspace);
    }

    #[test]
    fn failed_external_writer_closes_before_cleanup_and_preserves_target() {
        let workspace = TestWorkspace::new();
        let path = workspace.note("external.md", b"original");
        let version = open_versioned(&path).unwrap().1;
        let error = rewrite_external_file_versioned(&path, &version, |writer| {
            writer.write_all(b"partial replacement")?;
            Err(io::Error::other("injected writer failure"))
        })
        .unwrap_err();
        assert!(matches!(
            error,
            SaveError::PreCommit {
                stage: SaveStage::Write,
                ..
            }
        ));
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert_no_temp_files(&workspace);
    }

    #[test]
    fn failures_before_replace_preserve_original_and_cleanup_temp() {
        for stage in [
            SaveStage::Write,
            SaveStage::FileSync,
            SaveStage::ConflictCheck,
            SaveStage::Replace,
        ] {
            let workspace = TestWorkspace::new();
            let original = b"---\ntitle: Original\n---\nbody\n";
            let path = workspace.note("note.md", original);
            let mut checkpoint = FailAt { stage };
            let result = rewrite_metadata_with(
                &path,
                &MetadataPatch {
                    title: Some("Changed".to_owned()),
                    ..MetadataPatch::default()
                },
                &mut checkpoint,
            );
            assert!(matches!(
                result,
                Err(SaveError::PreCommit {
                    stage: failed_stage,
                    ..
                }) if failed_stage == stage
            ));
            assert_eq!(fs::read(&path).unwrap(), original);
            assert_no_temp_files(&workspace);
        }
    }

    #[test]
    fn post_replace_diagnostics_preserve_failure_messages() {
        for stage in ["CommittedInspect", "ParentCheckpoint", "ParentSync"] {
            let error = io::Error::from_raw_os_error(32);
            let message = error.to_string();
            assert_eq!(
                post_replace_failure(stage, Some(error)),
                SaveError::PostReplaceSync { message },
            );
            assert_eq!(
                post_replace_failure(stage, Some(io::Error::other("SYNTHETIC_SECRET"))),
                SaveError::PostReplaceSync {
                    message: "SYNTHETIC_SECRET".to_owned(),
                },
            );
        }
        assert_eq!(
            post_replace_failure("CommittedIdentity", None),
            SaveError::PostReplaceSync {
                message: "target changed immediately after atomic replace".to_owned(),
            },
        );
    }

    #[test]
    fn versioned_rewrite_reports_post_replace_failure_without_undoing_commit() {
        let workspace = TestWorkspace::new();
        let path = workspace.note("note.md", b"---\ntitle: Original\n---\nbody\n");
        let (_, version) = open_versioned(&path).unwrap();
        let result = rewrite_note_with(
            &path,
            &version,
            &MetadataPatch {
                deleted: Some(true),
                ..MetadataPatch::default()
            },
            |writer| writer.write_all(b"body\n"),
            &mut FailAt {
                stage: SaveStage::ParentSync,
            },
        );
        assert_eq!(
            result.unwrap_err(),
            SaveError::PostReplaceSync {
                message: "injected ParentSync failure".to_owned(),
            },
        );
        let output = fs::read_to_string(&path).unwrap();
        assert!(output.contains("deleted: true\n"));
        assert!(output.ends_with("body\n"));
        assert_no_temp_files(&workspace);
    }

    #[test]
    fn conflict_preserves_external_version_and_post_replace_failure_is_explicit() {
        let workspace = TestWorkspace::new();
        let path = workspace.note("note.md", b"---\ntitle: Original\n---\nbody\n");
        let external = b"---\ntitle: External\n---\nexternal body\n";
        let mut mutate = MutateAtConflict {
            path: path.clone(),
            content: external,
        };
        let result = rewrite_metadata_with(
            &path,
            &MetadataPatch {
                title: Some("Changed".to_owned()),
                ..MetadataPatch::default()
            },
            &mut mutate,
        );
        assert_eq!(result, Err(SaveError::Conflict));
        assert_eq!(fs::read(&path).unwrap(), external);
        assert_no_temp_files(&workspace);

        let mut fail_parent_sync = FailAt {
            stage: SaveStage::ParentSync,
        };
        let result = rewrite_metadata_with(
            &path,
            &MetadataPatch {
                title: Some("Committed despite sync error".to_owned()),
                ..MetadataPatch::default()
            },
            &mut fail_parent_sync,
        );
        assert!(matches!(result, Err(SaveError::PostReplaceSync { .. })));
        let output = fs::read_to_string(&path).unwrap();
        assert!(output.contains("title: 'Committed despite sync error'\n"));
        assert!(output.ends_with("external body\n"));
        assert_no_temp_files(&workspace);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_and_malformed_front_matter_without_writing() {
        use std::os::unix::fs::symlink;

        let workspace = TestWorkspace::new();
        let malformed = workspace.note("bad.md", b"---\ntags: scalar\n---\nbody\n");
        let original = fs::read(&malformed).unwrap();
        assert!(matches!(
            rewrite_metadata(
                &malformed,
                &MetadataPatch {
                    title: Some("No".to_owned()),
                    ..MetadataPatch::default()
                }
            ),
            Err(SaveError::Patch(_))
        ));
        assert_eq!(fs::read(&malformed).unwrap(), original);

        let outside = workspace.root.join("outside.md");
        fs::write(&outside, b"outside").unwrap();
        let linked = workspace.root.join("notes").join("linked.md");
        symlink(&outside, &linked).unwrap();
        assert!(matches!(
            rewrite_metadata(
                &linked,
                &MetadataPatch {
                    title: Some("No".to_owned()),
                    ..MetadataPatch::default()
                }
            ),
            Err(SaveError::InvalidTarget(_))
        ));
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
        assert_no_temp_files(&workspace);
    }

    #[test]
    fn synthetic_one_gigabyte_copy_uses_bounded_streaming() {
        let mut source = io::repeat(0).take(1_000_000_000);
        let mut destination = io::sink();
        assert_eq!(
            copy_bounded(&mut source, &mut destination).unwrap(),
            1_000_000_000
        );
    }

    fn assert_no_temp_files(workspace: &TestWorkspace) {
        let temp_count = fs::read_dir(workspace.root.join("notes"))
            .unwrap()
            .map(|entry| entry.expect("read temporary entry"))
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains(".stillus-tmp-")
            })
            .count();
        eprintln!("NATIVE_TEMP kind=Regular count={temp_count}");
        assert_eq!(temp_count, 0);
    }

    fn assert_no_secure_temp_files(workspace: &TestWorkspace) {
        let temp_count = fs::read_dir(workspace.root.join("notes"))
            .unwrap()
            .map(|entry| entry.expect("read secure temporary entry"))
            .filter(|entry| entry.file_name().to_str().is_some_and(is_secure_temp_name))
            .count();
        eprintln!("NATIVE_TEMP kind=Secure count={temp_count}");
        assert_eq!(temp_count, 0);
    }

    struct FailAt {
        stage: SaveStage,
    }

    impl Checkpoint for FailAt {
        fn check(&mut self, stage: SaveStage) -> io::Result<()> {
            if stage == self.stage {
                Err(io::Error::other(format!("injected {stage:?} failure")))
            } else {
                Ok(())
            }
        }
    }

    struct FailOperationAt {
        stage: OperationStage,
    }

    struct FailOperationOccurrence {
        stage: OperationStage,
        remaining: usize,
    }

    struct RepairProtectionAtPublish<'a> {
        workspace: &'a Path,
        remaining: usize,
        repaired: bool,
    }

    impl OperationCheckpoint for RepairProtectionAtPublish<'_> {
        fn check(&mut self, stage: OperationStage) -> Result<(), NoteOperationError> {
            if stage != OperationStage::Publish || self.repaired {
                return Ok(());
            }
            self.remaining = self.remaining.saturating_sub(1);
            if self.remaining == 0 {
                repair_workspace(self.workspace)
                    .map_err(|error| operation_failure(OperationStage::Publish, error))?;
                self.repaired = true;
            }
            Ok(())
        }
    }

    impl OperationCheckpoint for FailOperationOccurrence {
        fn check(&mut self, stage: OperationStage) -> Result<(), NoteOperationError> {
            if stage != self.stage {
                return Ok(());
            }
            self.remaining = self.remaining.saturating_sub(1);
            if self.remaining == 0 {
                Err(NoteOperationError::Failed {
                    stage,
                    message: format!("injected {stage:?} occurrence failure"),
                })
            } else {
                Ok(())
            }
        }
    }

    impl OperationCheckpoint for FailOperationAt {
        fn check(&mut self, stage: OperationStage) -> Result<(), NoteOperationError> {
            if stage == self.stage {
                Err(NoteOperationError::Failed {
                    stage,
                    message: format!("injected {stage:?} failure"),
                })
            } else {
                Ok(())
            }
        }
    }

    fn replace_test_contents(path: &Path, content: &[u8]) -> io::Result<()> {
        #[cfg(unix)]
        {
            fs::write(path, content)
        }
        #[cfg(windows)]
        {
            // Windows readers allow replacement but exclude an in-place writer.
            let temporary = path.with_extension("fault-injection");
            fs::write(&temporary, content)?;
            fs::rename(&temporary, path)
        }
    }

    struct MutateOperationAt<'a> {
        stage: OperationStage,
        path: PathBuf,
        content: &'a [u8],
    }

    impl OperationCheckpoint for MutateOperationAt<'_> {
        fn check(&mut self, stage: OperationStage) -> Result<(), NoteOperationError> {
            if stage == self.stage {
                replace_test_contents(&self.path, self.content)
                    .map_err(|error| operation_failure(stage, error))?;
            }
            Ok(())
        }
    }

    struct MutateAtConflict<'a> {
        path: PathBuf,
        content: &'a [u8],
    }

    impl Checkpoint for MutateAtConflict<'_> {
        fn check(&mut self, stage: SaveStage) -> io::Result<()> {
            if stage == SaveStage::ConflictCheck {
                replace_test_contents(&self.path, self.content)?;
            }
            Ok(())
        }
    }

    struct Lcg(u64);

    impl Lcg {
        fn new(seed: u64) -> Self {
            Self(seed)
        }

        fn next_usize(&mut self, upper: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((self.0 >> 32) as usize) % upper
        }
    }
}
