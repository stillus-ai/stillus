// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! Windows handles and metadata used by the portable storage implementation.

use std::fs as stdfs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::path::Path;
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use crate::diagnostics::{Operation, Stage, io_result};

pub use stdfs::{
    DirBuilder, DirEntry, FileTimes, ReadDir, canonicalize, copy, create_dir, create_dir_all,
    hard_link, read, read_dir, read_to_string, remove_dir, remove_dir_all, remove_file, write,
};

#[derive(Debug)]
pub struct File(stdfs::File);

impl File {
    pub fn options() -> OpenOptions {
        OpenOptions::new()
    }
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
            super::validate_real_path(parent)?;
        }
        Ok(Self(io_result(
            Operation::Metadata,
            Stage::Open,
            stdfs::OpenOptions::new()
                .read(true)
                .share_mode(1 | 4)
                .custom_flags(0x0200_0000 | 0x0020_0000)
                .open(path),
        )?))
    }
    pub fn create(path: impl AsRef<Path>) -> io::Result<Self> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
    }
    pub fn metadata(&self) -> io::Result<Metadata> {
        Metadata::from_file(&self.0)
    }
    pub fn try_clone(&self) -> io::Result<Self> {
        self.0.try_clone().map(Self)
    }
    pub fn set_permissions(&self, permissions: Permissions) -> io::Result<()> {
        super::windows::apply_permissions(&self.0, &permissions)
    }
}
impl std::ops::Deref for File {
    type Target = stdfs::File;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl Read for File {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.0.read(bytes)
    }
}
impl Read for &File {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        (&self.0).read(bytes)
    }
}
impl Write for File {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}
impl Write for &File {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        (&self.0).write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        (&self.0).flush()
    }
}
impl Seek for File {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.0.seek(position)
    }
}
impl Seek for &File {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        (&self.0).seek(position)
    }
}

#[derive(Clone, Debug)]
pub struct OpenOptions {
    inner: stdfs::OpenOptions,
    private: bool,
    exclusive_creation: bool,
}
impl Default for OpenOptions {
    fn default() -> Self {
        Self::new()
    }
}
impl OpenOptions {
    pub fn new() -> Self {
        let mut inner = stdfs::OpenOptions::new();
        inner.read(true).custom_flags(0x0020_0000);
        Self {
            inner,
            private: false,
            exclusive_creation: false,
        }
    }
    pub fn read(&mut self, value: bool) -> &mut Self {
        self.inner.read(value);
        self
    }
    pub fn write(&mut self, value: bool) -> &mut Self {
        self.inner.write(value);
        self
    }
    pub fn append(&mut self, value: bool) -> &mut Self {
        self.inner.append(value);
        self
    }
    pub fn truncate(&mut self, value: bool) -> &mut Self {
        self.inner.truncate(value);
        self
    }
    pub fn create(&mut self, value: bool) -> &mut Self {
        self.inner.create(value);
        self
    }
    pub fn create_new(&mut self, value: bool) -> &mut Self {
        self.inner.create_new(value);
        self.exclusive_creation = value;
        self
    }
    pub fn mode(&mut self, mode: u32) -> &mut Self {
        self.private = mode & 0o077 == 0;
        self
    }
    pub fn open(&self, path: impl AsRef<Path>) -> io::Result<File> {
        let path = path.as_ref();
        if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
            super::validate_real_path(parent)?;
        }
        if self.private {
            if !self.exclusive_creation {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "private creation requires create_new",
                ));
            }
            return super::create_private_file(path).map(File);
        }
        let mut options = self.inner.clone();
        if self.exclusive_creation {
            options.access_mode(0xc006_0000).share_mode(0);
        }
        options.open(path).map(File)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AccessRule {
    pub sid: String,
    pub allow: bool,
    pub flags: u8,
    pub mask: u32,
}
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Permissions {
    pub(crate) readonly: bool,
    pub(crate) rules: Vec<AccessRule>,
    pub(crate) private: bool,
}
impl Permissions {
    pub fn readonly(&self) -> bool {
        self.readonly
    }
    pub fn set_readonly(&mut self, value: bool) {
        self.readonly = value;
    }
    pub fn mode(&self) -> u32 {
        if self.private { 0o600 } else { 0o666 }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FileType {
    inner: stdfs::FileType,
    reparse: bool,
}
impl FileType {
    pub fn is_symlink(&self) -> bool {
        self.reparse
    }
    pub fn is_file(&self) -> bool {
        !self.reparse && self.inner.is_file()
    }
    pub fn is_dir(&self) -> bool {
        !self.reparse && self.inner.is_dir()
    }
}
#[derive(Clone, Debug)]
pub struct Metadata {
    inner: stdfs::Metadata,
    info: super::OpenFileInformation,
    permissions: Permissions,
    digest: [u8; 32],
}
impl Metadata {
    fn from_file(file: &stdfs::File) -> io::Result<Self> {
        let inner = io_result(Operation::Metadata, Stage::Inspect, file.metadata())?;
        let info = io_result(
            Operation::Metadata,
            Stage::Inspect,
            super::file_information(file),
        )?;
        let permissions = io_result(
            Operation::Metadata,
            Stage::Permissions,
            super::windows::capture_permissions(file),
        )?;
        let mut hasher = Sha256::new();
        // Windows has no safe change-time accessor in the pinned handle API.
        // Hash through this handle to detect same-size writes with restored mtime.
        // The reader excludes concurrent writers and uses bounded memory.
        if inner.is_file() && !super::is_link(&inner) {
            // Unlike Unix pread, Windows seek_read changes the shared cursor.
            // Metadata must leave the caller's next read/write at its original offset,
            // including when hashing fails partway through the file.
            let mut reader = file;
            let position = reader.stream_position()?;
            let hashed = (|| -> io::Result<()> {
                let mut offset = 0;
                let mut buffer = [0; 65_536];
                loop {
                    let count = file.seek_read(&mut buffer, offset)?;
                    if count == 0 {
                        break;
                    }
                    hasher.update(&buffer[..count]);
                    offset += count as u64;
                }
                Ok(())
            })();
            let restored = reader.seek(SeekFrom::Start(position));
            io_result(Operation::Metadata, Stage::Hash, hashed)?;
            io_result(Operation::Metadata, Stage::Restore, restored)?;
        }
        Ok(Self {
            inner,
            info,
            permissions,
            digest: hasher.finalize().into(),
        })
    }
    pub fn file_type(&self) -> FileType {
        FileType {
            inner: self.inner.file_type(),
            reparse: super::is_link(&self.inner),
        }
    }
    pub fn is_dir(&self) -> bool {
        self.file_type().is_dir()
    }
    pub fn is_file(&self) -> bool {
        self.file_type().is_file()
    }
    pub fn len(&self) -> u64 {
        self.inner.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn modified(&self) -> io::Result<SystemTime> {
        self.inner.modified()
    }
    pub fn created(&self) -> io::Result<SystemTime> {
        self.inner.created()
    }
    pub fn permissions(&self) -> Permissions {
        self.permissions.clone()
    }
    pub fn dev(&self) -> u64 {
        match self.info.identity {
            super::FileIdentity::Windows { volume, .. } => volume,
            _ => unreachable!(),
        }
    }
    pub fn ino(&self) -> u64 {
        match self.info.identity {
            super::FileIdentity::Windows { index, .. } => index,
            _ => unreachable!(),
        }
    }
    pub fn mtime(&self) -> i64 {
        self.modified()
            .ok()
            .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map_or(0, |time| time.as_secs() as i64)
    }
    pub fn mtime_nsec(&self) -> i64 {
        self.modified()
            .ok()
            .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map_or(0, |time| time.subsec_nanos() as i64)
    }
    pub fn ctime(&self) -> i64 {
        self.mtime()
    }
    pub fn ctime_nsec(&self) -> i64 {
        self.mtime_nsec()
    }
    pub fn nlink(&self) -> u64 {
        self.info.links
    }
    pub fn identity(&self) -> super::FileIdentity {
        self.info.identity
    }
    pub fn digest(&self) -> [u8; 32] {
        self.digest
    }
}

pub fn symlink_metadata(path: impl AsRef<Path>) -> io::Result<Metadata> {
    File::open(path)?.metadata()
}
pub fn metadata(path: impl AsRef<Path>) -> io::Result<Metadata> {
    symlink_metadata(path)
}
pub fn rename(source: impl AsRef<Path>, destination: impl AsRef<Path>) -> io::Result<()> {
    super::replace(source.as_ref(), destination.as_ref())
}
pub fn set_permissions(path: impl AsRef<Path>, permissions: Permissions) -> io::Result<()> {
    let file = stdfs::OpenOptions::new()
        .access_mode(0x0006_0100)
        .custom_flags(0x0220_0000)
        .open(path)?;
    super::windows::apply_permissions(&file, &permissions)
}
