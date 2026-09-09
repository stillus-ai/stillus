// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! File operations shared by every persistent Stillus subsystem.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::fs::{self as std_fs, File};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[cfg(feature = "credentials")]
pub mod credentials;
pub mod diagnostics;
mod operation_lock;
#[cfg(any(windows, test))]
mod sync_marker;
pub use operation_lock::{ActivityLease, OperationLock};

#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use std::fs;
#[cfg(windows)]
#[path = "windows_fs.rs"]
pub mod fs;

/// Serialized identities never compare equal across operating systems.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "platform", rename_all = "snake_case")]
pub enum FileIdentity {
    Unix { device: u64, inode: u64 },
    Windows { volume: u64, index: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenFileInformation {
    pub identity: FileIdentity,
    pub links: u64,
}

/// Inspect the open handle, never a second lookup of its filename.
pub fn file_information(file: &File) -> io::Result<OpenFileInformation> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        Ok(OpenFileInformation {
            identity: FileIdentity::Unix {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            links: metadata.nlink(),
        })
    }
    #[cfg(windows)]
    {
        let info = winapi_util::file::information(file)?;
        Ok(OpenFileInformation {
            identity: FileIdentity::Windows {
                volume: info.volume_serial_number(),
                index: info.file_index(),
            },
            links: info.number_of_links(),
        })
    }
}

pub fn is_link(metadata: &std_fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        metadata.file_type().is_symlink()
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Includes junctions and every other kind of reparse point.
        metadata.file_attributes() & 0x400 != 0
    }
}

/// Reject links in every existing component, including the workspace root.
pub fn validate_real_path(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    for component in path.components() {
        if let std::path::Component::Prefix(prefix) = component {
            if !matches!(
                prefix.kind(),
                std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)
            ) {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "only local Windows drive paths are supported",
                ));
            }
        }
    }
    for ancestor in path.ancestors() {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        let metadata = std_fs::symlink_metadata(ancestor)?;
        if is_link(&metadata) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path contains a link or reparse point",
            ));
        }
    }
    Ok(())
}

/// Create an empty, exclusive file; restrict access before returning a writer.
pub fn create_private_file(path: &Path) -> io::Result<File> {
    #[cfg(windows)]
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file has no parent"))?;
    #[cfg(windows)]
    validate_real_path(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(windows)]
    {
        windows::create_private_file(path)
    }
}

pub fn create_private_directory(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "directory has no parent"))?;
    #[cfg(windows)]
    validate_real_path(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std_fs::DirBuilder::new().mode(0o700).create(path)?;
        File::open(parent)?.sync_all()
    }
    #[cfg(windows)]
    {
        windows::create_private_directory(path)
    }
}

pub fn validate_private(path: &Path) -> io::Result<()> {
    validate_real_path(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = std_fs::symlink_metadata(path)?;
        if metadata.mode() & 0o077 != 0 || (metadata.is_file() && metadata.nlink() != 1) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "private path has shared access",
            ));
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        windows::validate_private(path)
    }
}

/// Copy permissions through handles, so a replaced path cannot redirect ACL changes.
pub fn preserve_permissions(source: &File, destination: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        destination.set_permissions(source.metadata()?.permissions())
    }
    #[cfg(windows)]
    {
        windows::preserve_permissions(source, destination)
    }
}

/// Atomic replacement. Callers must close writers and their own Windows target
/// readers first, and retain recovery on error.
/// Unix callers perform their existing parent-directory sync after this operation.
/// Windows uses MoveFileExW with WRITE_THROUGH, including its error result.
pub fn replace(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std_fs::rename(source, destination)
    }
    #[cfg(windows)]
    {
        diagnostics::io_result(
            diagnostics::Operation::Replace,
            diagnostics::Stage::Publish,
            atomicwrites::replace_atomic(source, destination),
        )
    }
}

/// Publish a complete temporary file without replacing an existing destination.
/// Unix keeps the temporary link until the caller removes it and syncs the parent.
/// Windows moves the temporary file with WRITE_THROUGH and without REPLACE_EXISTING.
pub fn publish(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std_fs::hard_link(source, destination)
    }
    #[cfg(windows)]
    {
        atomicwrites::move_atomic(source, destination)
    }
}

/// Flush a completed writable file. A read-only handle is insufficient on Windows.
pub fn sync_file(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(windows)]
    {
        OpenOptions::new().write(true).open(path)?.sync_all()
    }
}

/// Commit a namespace barrier after creation, linking, or removal.
pub fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(windows)]
    {
        windows::sync_directory(path)
    }
}

pub fn home_directory() -> Option<PathBuf> {
    home_directory_from(|key| std::env::var_os(key))
}

fn home_directory_from(mut lookup: impl FnMut(&str) -> Option<OsString>) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        lookup("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    }
    #[cfg(windows)]
    {
        if let Some(profile) = lookup("USERPROFILE").filter(|value| !value.is_empty()) {
            let path = PathBuf::from(profile);
            if path.is_absolute() {
                return Some(path);
            }
        }
        let mut drive = lookup("HOMEDRIVE")?;
        let path = lookup("HOMEPATH")?;
        if drive.is_empty() || path.is_empty() {
            return None;
        }
        drive.push(path);
        let path = PathBuf::from(drive);
        path.is_absolute().then_some(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    pub(super) struct TestDirectory(pub(super) PathBuf);
    impl TestDirectory {
        pub(super) fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "stillus platform 日本語 {} {}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            create_private_directory(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            std_fs::remove_dir_all(&self.0).unwrap();
        }
    }
    #[test]
    fn identity_tracks_open_file_and_hard_links() {
        let directory = TestDirectory::new();
        let path = directory.0.join("note.md");
        std_fs::write(&path, b"original").unwrap();
        let opened = File::open(&path).unwrap();
        let before = file_information(&opened).unwrap();
        let link = directory.0.join("link.md");
        std_fs::hard_link(&path, &link).unwrap();
        let after = file_information(&File::open(&link).unwrap()).unwrap();
        assert_eq!(before.identity, after.identity);
        assert_eq!(after.links, 2);
        let temporary = directory.0.join("replacement.md");
        std_fs::write(&temporary, b"replacement").unwrap();
        #[cfg(windows)]
        {
            // MoveFileEx cannot replace this still-open target. It must leave
            // both files intact so the caller can close its reader and retry.
            assert!(replace(&temporary, &path).is_err());
            assert_eq!(std_fs::read(&path).unwrap(), b"original");
            assert_eq!(std_fs::read(&temporary).unwrap(), b"replacement");
            assert_eq!(file_information(&opened).unwrap().identity, before.identity);
            drop(opened);
        }
        replace(&temporary, &path).unwrap();
        #[cfg(unix)]
        assert_eq!(file_information(&opened).unwrap().identity, before.identity);
        assert_eq!(
            file_information(&File::open(link).unwrap())
                .unwrap()
                .identity,
            before.identity
        );
        assert_ne!(
            file_information(&File::open(path).unwrap())
                .unwrap()
                .identity,
            before.identity
        );
    }
    #[test]
    fn private_creation_and_publication_never_overwrite() {
        let directory = TestDirectory::new();
        validate_private(&directory.0).unwrap();
        let temporary = directory.0.join("temporary");
        let mut file = create_private_file(&temporary).unwrap();
        file.write_all(b"private").unwrap();
        file.sync_all().unwrap();
        drop(file);
        validate_private(&temporary).unwrap();
        assert!(create_private_file(&temporary).is_err());
        let destination = directory.0.join("destination");
        std_fs::write(&destination, b"existing").unwrap();
        assert!(publish(&temporary, &destination).is_err());
        assert_eq!(std_fs::read(&destination).unwrap(), b"existing");
        assert_eq!(std_fs::read(&temporary).unwrap(), b"private");
        std_fs::remove_file(&destination).unwrap();
        publish(&temporary, &destination).unwrap();
        assert_eq!(std_fs::read(&destination).unwrap(), b"private");
    }
    #[test]
    fn serialized_identities_are_platform_specific() {
        assert_ne!(
            FileIdentity::Unix {
                device: 1,
                inode: 2
            },
            FileIdentity::Windows {
                volume: 1,
                index: 2
            }
        );
    }

    #[test]
    fn metadata_preserves_the_open_read_and_write_position() {
        use std::io::{Read, Seek, SeekFrom};
        let directory = TestDirectory::new();
        let path = directory.0.join("cursor.md");
        std_fs::write(&path, b"0123456789").unwrap();
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.seek(SeekFrom::Start(3)).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 10);
        assert_eq!(file.stream_position().unwrap(), 3);
        let mut bytes = [0; 2];
        file.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"34");
        file.metadata().unwrap();
        file.write_all(b"XX").unwrap();
        drop(file);
        assert_eq!(std_fs::read(path).unwrap(), b"01234XX789");
    }

    #[test]
    fn replacement_preserves_inherited_permissions_through_handles() {
        let directory = TestDirectory::new();
        let source = directory.0.join("inherited.md");
        let candidate = directory.0.join("candidate.md");
        std_fs::write(&source, b"original").unwrap();
        let expected = fs::metadata(&source).unwrap().permissions();
        #[cfg(windows)]
        {
            assert!(expected.rules.len() >= 3);
            assert!(expected.rules.iter().all(|rule| rule.flags & 0x10 != 0));
        }
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        let candidate_file = options.open(&candidate).unwrap();
        candidate_file.set_permissions(expected.clone()).unwrap();
        drop(candidate_file);
        #[cfg(windows)]
        assert_eq!(fs::metadata(&candidate).unwrap().permissions(), expected);
    }

    #[cfg(windows)]
    #[test]
    fn replacement_preserves_inherited_permissions_on_private_temporary() {
        let directory = TestDirectory::new();
        let source = directory.0.join("inherited.md");
        let candidate = directory.0.join("private.tmp");
        std_fs::write(&source, b"original").unwrap();
        let expected = fs::metadata(&source).unwrap().permissions();
        assert!(expected.rules.len() >= 3);
        assert!(expected.rules.iter().all(|rule| rule.flags & 0x10 != 0));

        let mut candidate_file = create_private_file(&candidate).unwrap();
        windows::apply_permissions(&candidate_file, &expected).unwrap();
        // The existing exclusive handle must still exclude other readers after
        // inheritance changes, until the complete replacement is ready.
        assert!(std_fs::read(&candidate).is_err());
        candidate_file.write_all(b"replacement").unwrap();
        candidate_file.sync_all().unwrap();
        drop(candidate_file);
        assert_eq!(fs::metadata(&candidate).unwrap().permissions(), expected);
        replace(&candidate, &source).unwrap();
        assert_eq!(std_fs::read(&source).unwrap(), b"replacement");
        assert_eq!(fs::metadata(&source).unwrap().permissions(), expected);
        assert!(!candidate.exists());
    }

    #[cfg(windows)]
    #[test]
    fn inherited_permissions_reject_a_different_parent_acl() {
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        use windows_acl::acl::ACL;
        use windows_acl::helper::string_to_sid;

        let directory = TestDirectory::new();
        let source = directory.0.join("source.md");
        std_fs::write(&source, b"original").unwrap();
        let expected = fs::metadata(&source).unwrap().permissions();
        let other_parent = directory.0.join("different parent");
        create_private_directory(&other_parent).unwrap();
        let parent = OpenOptions::new()
            .access_mode(0x0006_0000)
            .custom_flags(0x0220_0000)
            .open(&other_parent)
            .unwrap();
        let mut acl = ACL::from_file_handle(parent.as_raw_handle().cast(), false).unwrap();
        let guests = string_to_sid("S-1-5-32-546").unwrap();
        assert!(
            acl.allow(guests.as_ptr().cast_mut().cast(), true, 0x0002_0000)
                .unwrap()
        );
        drop(parent);

        let candidate = other_parent.join("private.tmp");
        let candidate_file = create_private_file(&candidate).unwrap();
        assert_eq!(
            windows::apply_permissions(&candidate_file, &expected)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(candidate_file.metadata().unwrap().len(), 0);
        drop(candidate_file);
        std_fs::remove_file(candidate).unwrap();
        assert_eq!(std_fs::read(&source).unwrap(), b"original");
        assert_eq!(fs::metadata(&source).unwrap().permissions(), expected);
    }

    #[cfg(windows)]
    #[test]
    fn replacement_preserves_mixed_and_noncanonical_acl_order() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_permissions::constants::{SeObjectType, SecurityInformation};
        use windows_permissions::{LocalBox, SecurityDescriptor, wrappers};

        let directory = TestDirectory::new();
        let source = directory.0.join("mixed.md");
        std_fs::write(&source, b"original").unwrap();
        // Build the source fixture directly through the dependency, independently
        // of our permission copier. Keep inherited rules from the private parent.
        let mut source_file = OpenOptions::new()
            .access_mode(0x0006_0000)
            .open(&source)
            .unwrap();
        let inherited = wrappers::GetSecurityInfo(
            &source_file,
            SeObjectType::SE_FILE_OBJECT,
            SecurityInformation::Dacl,
        )
        .unwrap();
        let inherited = wrappers::ConvertSecurityDescriptorToStringSecurityDescriptor(
            &inherited,
            SecurityInformation::Dacl,
        )
        .unwrap()
        .into_string()
        .unwrap();
        let entries = &inherited[inherited.find('(').unwrap()..];
        // Duplicate explicit SYSTEM entries must not be merged or overwrite
        // each other, nor the inherited SYSTEM entry that follows them.
        let mixed: LocalBox<SecurityDescriptor> =
            format!("D:(D;;0x2;;;BG)(A;;0x20000;;;SY)(A;;0x100000;;;SY){entries}")
                .parse()
                .unwrap();
        wrappers::SetSecurityInfo(
            &mut source_file,
            SeObjectType::SE_FILE_OBJECT,
            SecurityInformation::Dacl,
            None,
            None,
            Some(mixed.dacl().unwrap()),
            None,
        )
        .unwrap();
        drop(source_file);
        let expected = fs::metadata(&source).unwrap().permissions();
        assert!(expected.rules.len() >= 6);
        assert!(!expected.rules[0].allow);
        assert!(expected.rules[1].allow);
        assert!(
            expected.rules[..3]
                .iter()
                .all(|rule| rule.flags & 0x10 == 0)
        );
        assert!(
            expected.rules[3..]
                .iter()
                .all(|rule| rule.flags & 0x10 != 0)
        );

        let candidate = directory.0.join("candidate.md");
        let candidate_file = create_private_file(&candidate).unwrap();
        let source_file = File::open(&source).unwrap();
        preserve_permissions(&source_file, &candidate_file).unwrap();
        drop(source_file);
        drop(candidate_file);
        assert_eq!(fs::metadata(&candidate).unwrap().permissions(), expected);

        // Unlike the old per-entry API, a complete DACL installation must also
        // preserve access-check order when explicit rules are not canonical.
        let rejected = directory.0.join("rejected.md");
        let rejected_file = create_private_file(&rejected).unwrap();
        let mut noncanonical = expected;
        noncanonical.rules.swap(0, 1);
        windows::apply_permissions(&rejected_file, &noncanonical).unwrap();
        let mut unsupported = noncanonical.clone();
        unsupported.rules[0].flags |= 0x20;
        assert_eq!(
            windows::apply_permissions(&rejected_file, &unsupported)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Unsupported
        );
        drop(rejected_file);
        assert_eq!(fs::metadata(&rejected).unwrap().permissions(), noncanonical);
        std_fs::remove_file(rejected).unwrap();
        assert_eq!(std_fs::read(source).unwrap(), b"original");
    }
    #[test]
    fn replacement_preserves_private_permissions_through_handles() {
        let directory = TestDirectory::new();
        let source = directory.0.join("source.md");
        let candidate = directory.0.join("candidate.md");
        let source_file = create_private_file(&source).unwrap();
        let candidate_file = create_private_file(&candidate).unwrap();
        preserve_permissions(&source_file, &candidate_file).unwrap();
        candidate_file.sync_all().unwrap();
        drop(candidate_file);
        drop(source_file);
        replace(&candidate, &source).unwrap();
        validate_private(&source).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_exclusive_private_file_cannot_be_read_before_close() {
        let directory = TestDirectory::new();
        let path = directory.0.join("private.md");
        let mut file = create_private_file(&path).unwrap();
        file.write_all(b"private body").unwrap();
        assert!(std_fs::read(&path).is_err());
        file.sync_all().unwrap();
        drop(file);
        validate_private(&path).unwrap();
        assert_eq!(std_fs::read(&path).unwrap(), b"private body");
    }

    #[cfg(windows)]
    #[test]
    fn windows_failed_replace_preserves_locked_and_readonly_targets() {
        use std::os::windows::fs::OpenOptionsExt;
        let directory = TestDirectory::new();
        let destination = directory.0.join("target.md");
        let candidate = directory.0.join("candidate.md");
        std_fs::write(&destination, b"original").unwrap();
        std_fs::write(&candidate, b"candidate").unwrap();
        let lock = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&destination)
            .unwrap();
        assert!(replace(&candidate, &destination).is_err());
        drop(lock);
        assert_eq!(std_fs::read(&destination).unwrap(), b"original");
        let original_permissions = std_fs::metadata(&destination).unwrap().permissions();
        let mut permissions = original_permissions.clone();
        permissions.set_readonly(true);
        std_fs::set_permissions(&destination, permissions).unwrap();
        assert!(replace(&candidate, &destination).is_err());
        assert_eq!(std_fs::read(&candidate).unwrap(), b"candidate");
        std_fs::set_permissions(&destination, original_permissions).unwrap();
        replace(&candidate, &destination).unwrap();
        assert_eq!(std_fs::read(&destination).unwrap(), b"candidate");
    }

    #[cfg(windows)]
    #[test]
    fn readonly_source_cannot_make_an_uncleanable_temporary() {
        let directory = TestDirectory::new();
        let source = directory.0.join("source.md");
        let candidate = directory.0.join("candidate.md");
        drop(create_private_file(&source).unwrap());
        let original_attributes = std_fs::metadata(&source).unwrap().permissions();
        let mut attributes = original_attributes.clone();
        attributes.set_readonly(true);
        std_fs::set_permissions(&source, attributes).unwrap();
        let source_file = File::open(&source).unwrap();
        let candidate_file = create_private_file(&candidate).unwrap();
        assert_eq!(
            preserve_permissions(&source_file, &candidate_file)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(candidate_file.metadata().unwrap().len(), 0);
        drop(candidate_file);
        std_fs::remove_file(&candidate).unwrap();
        drop(source_file);
        std_fs::set_permissions(&source, original_attributes).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_junction_is_rejected_including_ancestor_components() {
        let Some(path) = std::env::var_os("STILLUS_TEST_JUNCTION") else {
            panic!(
                "run the Windows test kit through Run-Tests.ps1 to provision the NTFS junction fixture"
            );
        };
        let path = PathBuf::from(path);
        assert!(is_link(&std_fs::symlink_metadata(&path).unwrap()));
        assert!(validate_real_path(&path).is_err());
        assert!(create_private_file(&path.join("must not exist.md")).is_err());
        assert!(!path.join("must not exist.md").exists());
    }

    #[test]
    fn namespace_barrier_does_not_leave_records_on_success() {
        let directory = TestDirectory::new();
        sync_directory(&directory.0).unwrap();
        assert_eq!(std_fs::read_dir(&directory.0).unwrap().count(), 0);
    }

    #[cfg(windows)]
    #[test]
    fn home_uses_profile_then_drive_and_path() {
        let values = [
            ("USERPROFILE", "C:\\Users\\日本語 Name"),
            ("HOMEDRIVE", "D:"),
            ("HOMEPATH", "\\Fallback"),
        ];
        let lookup = |key: &str| {
            values
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| OsString::from(value))
        };
        assert_eq!(
            home_directory_from(lookup),
            Some(PathBuf::from(values[0].1))
        );
        assert_eq!(
            home_directory_from(|key| if key == "USERPROFILE" {
                None
            } else {
                lookup(key)
            }),
            Some(PathBuf::from("D:\\Fallback"))
        );
        assert_eq!(home_directory_from(|_| None), None);
    }
}
