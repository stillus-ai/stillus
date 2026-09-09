// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! Extraction of release packages into a staging directory.
//!
//! Package contents are untrusted until [`crate::payload`] has checked them,
//! so extraction refuses anything that is not a plain file or directory with a
//! relative, well formed path, and stops at fixed size limits.

use crate::UpdateError;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

const MAX_ENTRIES: usize = 20_000;
const MAX_TOTAL_BYTES: u64 = 600 * 1024 * 1024;
const MAX_ENTRY_BYTES: u64 = 400 * 1024 * 1024;
const MAX_COMPONENTS: usize = 24;
const MAX_COMPONENT_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveKind {
    /// A gzip compressed tar whose entries live under a single root directory.
    TarGz,
    /// A zip whose entries are stored at the archive root.
    Zip,
}

impl ArchiveKind {
    pub fn for_name(name: &str) -> Option<Self> {
        if name.ends_with(".tar.gz") {
            Some(Self::TarGz)
        } else if name.ends_with(".zip") {
            Some(Self::Zip)
        } else {
            None
        }
    }

    /// Leading path components dropped from every entry.
    fn strip(self) -> usize {
        match self {
            Self::TarGz => 1,
            Self::Zip => 0,
        }
    }
}

pub(crate) fn extract(
    bytes: &[u8],
    kind: ArchiveKind,
    destination: &Path,
) -> Result<(), UpdateError> {
    match kind {
        ArchiveKind::TarGz => extract_tar(bytes, kind.strip(), destination),
        ArchiveKind::Zip => extract_zip(bytes, kind.strip(), destination),
    }
}

fn extract_tar(bytes: &[u8], strip: usize, destination: &Path) -> Result<(), UpdateError> {
    let decoder = flate2::read::GzDecoder::new(Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    let mut budget = Budget::default();
    let entries = archive
        .entries()
        .map_err(|_| UpdateError::Package("unreadable archive"))?;
    for entry in entries {
        let entry = entry.map_err(|_| UpdateError::Package("unreadable archive entry"))?;
        let header = entry.header();
        let kind = header.entry_type();
        if kind.is_pax_global_extensions()
            || kind.is_pax_local_extensions()
            || kind.is_gnu_longname()
        {
            continue;
        }
        let size = header.size().unwrap_or(u64::MAX);
        let mode = header.mode().unwrap_or(0o644);
        let raw = entry.path_bytes();
        let raw = std::str::from_utf8(&raw)
            .map_err(|_| UpdateError::Package("archive path is not UTF-8"))?;
        let Some(relative) = relative(raw, strip)? else {
            continue;
        };
        let path = destination.join(relative);
        if kind.is_dir() {
            budget.entry()?;
            fs::create_dir_all(&path)?;
            continue;
        }
        if !kind.is_file() {
            return Err(UpdateError::Package("archive contains a special file"));
        }
        budget.file(size)?;
        let mut data = Vec::new();
        entry
            .take(size.min(MAX_ENTRY_BYTES) + 1)
            .read_to_end(&mut data)
            .map_err(|_| UpdateError::Package("truncated archive entry"))?;
        if data.len() as u64 != size {
            return Err(UpdateError::Package("archive entry size mismatch"));
        }
        write_file(&path, &data, Some(mode))?;
    }
    Ok(())
}

fn extract_zip(bytes: &[u8], strip: usize, destination: &Path) -> Result<(), UpdateError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|_| UpdateError::Package("unreadable archive"))?;
    let mut budget = Budget::default();
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|_| UpdateError::Package("unreadable archive entry"))?;
        let name = std::str::from_utf8(entry.name_raw())
            .map_err(|_| UpdateError::Package("archive path is not UTF-8"))?;
        let is_dir = entry.is_dir();
        let size = entry.size();
        let mode = entry.unix_mode();
        let Some(relative) = relative(name, strip)? else {
            continue;
        };
        let path = destination.join(relative);
        if is_dir {
            budget.entry()?;
            fs::create_dir_all(&path)?;
            continue;
        }
        if !entry.is_file() {
            return Err(UpdateError::Package("archive contains a special file"));
        }
        budget.file(size)?;
        let mut data = Vec::new();
        entry
            .by_ref()
            .take(size.min(MAX_ENTRY_BYTES) + 1)
            .read_to_end(&mut data)
            .map_err(|_| UpdateError::Package("truncated archive entry"))?;
        if data.len() as u64 != size {
            return Err(UpdateError::Package("archive entry size mismatch"));
        }
        write_file(&path, &data, mode)?;
    }
    Ok(())
}

#[derive(Default)]
struct Budget {
    entries: usize,
    bytes: u64,
}

impl Budget {
    fn entry(&mut self) -> Result<(), UpdateError> {
        self.entries += 1;
        if self.entries > MAX_ENTRIES {
            return Err(UpdateError::Package("archive has too many entries"));
        }
        Ok(())
    }

    fn file(&mut self, size: u64) -> Result<(), UpdateError> {
        self.entry()?;
        if size > MAX_ENTRY_BYTES {
            return Err(UpdateError::Package("archive entry is too large"));
        }
        self.bytes = self.bytes.saturating_add(size);
        if self.bytes > MAX_TOTAL_BYTES {
            return Err(UpdateError::Package("archive is too large"));
        }
        Ok(())
    }
}

fn write_file(path: &Path, data: &[u8], mode: Option<u32>) -> Result<(), UpdateError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::File::create(path)?;
    file.write_all(data)?;
    file.sync_all()?;
    drop(file);
    set_mode(path, mode)?;
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: Option<u32>) -> Result<(), UpdateError> {
    use std::os::unix::fs::PermissionsExt;
    // Keep the executable bit the publisher set, drop everything else: a
    // package must never introduce setuid, setgid or group writable files.
    let executable = mode.is_some_and(|mode| mode & 0o100 != 0);
    let permissions = fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 });
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: Option<u32>) -> Result<(), UpdateError> {
    Ok(())
}

/// Validates one archive path and removes `strip` leading components.
/// Returns `None` for the stripped root itself.
pub(crate) fn relative(raw: &str, strip: usize) -> Result<Option<PathBuf>, UpdateError> {
    // Archive names are POSIX text, not host paths. Windows components() and
    // ZIP enclosed_name() can erase forbidden separators or parent components.
    if raw.starts_with('/') || raw.contains(['\\', ':']) {
        return Err(UpdateError::Package("disallowed archive path"));
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in raw.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part.len() > MAX_COMPONENT_BYTES
            || part == ".."
            || part.chars().any(char::is_control)
            || part.ends_with(' ')
            || part.ends_with('.')
        {
            return Err(UpdateError::Package("disallowed archive path"));
        }
        parts.push(part);
        if parts.len() > MAX_COMPONENTS {
            return Err(UpdateError::Package("archive path is too deep"));
        }
    }
    if parts.len() <= strip {
        return Ok(None);
    }
    Ok(Some(parts[strip..].iter().collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_gz(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, data, mode) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(*mode);
            header.set_cksum();
            builder.append_data(&mut header, name, *data).unwrap();
        }
        gzip(&builder.into_inner().unwrap())
    }

    fn gzip(raw: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(raw).unwrap();
        encoder.finish().unwrap()
    }

    /// One hand written ustar record, so that tests can carry names the tar
    /// writer refuses to produce.
    fn raw_tar(name: &str, data: &[u8]) -> Vec<u8> {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        header[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[148..156].copy_from_slice(b"        ");
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let sum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
        header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        let mut archive = header.to_vec();
        archive.extend_from_slice(data);
        archive.resize(archive.len() + (512 - data.len() % 512) % 512, 0);
        archive.resize(archive.len() + 1024, 0);
        archive
    }

    #[test]
    fn archive_kinds_follow_the_published_names() {
        assert_eq!(
            ArchiveKind::for_name("stillus-1.0.0-linux-x86_64.tar.gz"),
            Some(ArchiveKind::TarGz)
        );
        assert_eq!(
            ArchiveKind::for_name("stillus-1.0.0-windows-x86_64.zip"),
            Some(ArchiveKind::Zip)
        );
        assert_eq!(ArchiveKind::for_name("stillus.dmg"), None);
    }

    #[test]
    fn the_root_directory_is_stripped() {
        assert_eq!(
            relative("stillus-linux-x86_64/stillus", 1).unwrap(),
            Some(PathBuf::from("stillus"))
        );
        assert_eq!(relative("stillus-linux-x86_64", 1).unwrap(), None);
        assert_eq!(
            relative("./root/a/b.txt", 1).unwrap(),
            Some(PathBuf::from("a/b.txt"))
        );
    }

    fn zip_file(name: &str, data: &[u8]) -> Vec<u8> {
        let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
        archive
            .start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(data).unwrap();
        archive.finish().unwrap().into_inner()
    }

    fn assert_rejected(name: &str) {
        assert!(relative(name, 1).is_err());
        for (archive, kind) in [
            (gzip(&raw_tar(name, b"untrusted")), ArchiveKind::TarGz),
            (zip_file(name, b"untrusted"), ArchiveKind::Zip),
        ] {
            let parent = tempfile::tempdir().unwrap();
            let staged = parent.path().join("staged");
            fs::create_dir(&staged).unwrap();
            assert!(extract(&archive, kind, &staged).is_err(), "{kind:?}");
            assert_eq!(fs::read_dir(&staged).unwrap().count(), 0);
            assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn archive_paths_reject_backslashes_before_host_normalization() {
        assert_rejected("root/a\\b");
    }

    #[test]
    fn archive_paths_reject_mixed_separator_traversal() {
        assert_rejected("root/a/..\\escape");
    }

    #[test]
    fn archive_paths_reject_internal_parent_components() {
        assert_rejected("root/a/../escape");
    }

    #[test]
    fn archive_paths_reject_absolute_paths() {
        assert_rejected("/root/escape");
    }

    #[test]
    fn archive_paths_reject_windows_drive_and_stream_prefixes() {
        for name in ["C:/root/escape", "C:escape", "root/a:b"] {
            assert_rejected(name);
        }
    }

    #[test]
    fn archive_paths_reject_unc_and_verbatim_prefixes() {
        for name in [
            "\\\\server\\share\\escape",
            "\\\\?\\C:\\escape",
            "//server/share/escape",
        ] {
            assert_rejected(name);
        }
    }

    #[test]
    fn archive_paths_preserve_unicode_and_initial_dot() {
        for (archive, kind, expected) in [
            (
                gzip(&raw_tar("./root/日本語/file.txt", b"fixture")),
                ArchiveKind::TarGz,
                "日本語/file.txt",
            ),
            (
                zip_file("./日本語/file.txt", b"fixture"),
                ArchiveKind::Zip,
                "日本語/file.txt",
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            extract(&archive, kind, directory.path()).unwrap();
            assert_eq!(
                fs::read(directory.path().join(expected)).unwrap(),
                b"fixture"
            );
        }
    }

    #[test]
    fn archive_path_limits_and_invalid_components_are_preserved() {
        for name in ["root/trailing.", "root/trailing ", "root/control\n"] {
            assert_rejected(name);
        }
        assert!(relative(&"a".repeat(MAX_COMPONENT_BYTES + 1), 0).is_err());
        assert!(relative(&vec!["a"; MAX_COMPONENTS + 1].join("/"), 0).is_err());
    }

    #[test]
    fn tar_entries_land_in_the_staging_directory() {
        let directory = tempfile::tempdir().unwrap();
        let archive = tar_gz(&[
            ("stillus-linux-x86_64/stillus", b"binary", 0o755),
            ("stillus-linux-x86_64/LICENSE.txt", b"license", 0o644),
            ("stillus-linux-x86_64/nested/build.json", b"{}", 0o644),
        ]);
        extract(&archive, ArchiveKind::TarGz, directory.path()).unwrap();
        assert_eq!(
            fs::read(directory.path().join("stillus")).unwrap(),
            b"binary".to_vec()
        );
        assert_eq!(
            fs::read(directory.path().join("nested/build.json")).unwrap(),
            b"{}".to_vec()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |name: &str| {
                fs::metadata(directory.path().join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777
            };
            assert_eq!(mode("stillus"), 0o755);
            assert_eq!(mode("LICENSE.txt"), 0o644);
        }
    }

    #[test]
    fn symbolic_links_and_absolute_paths_are_refused() {
        let directory = tempfile::tempdir().unwrap();
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o777);
        header.set_entry_type(tar::EntryType::Symlink);
        builder
            .append_link(&mut header, "root/link", "/etc/passwd")
            .unwrap();
        let raw = builder.into_inner().unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&raw).unwrap();
        let archive = encoder.finish().unwrap();
        assert_eq!(
            extract(&archive, ArchiveKind::TarGz, directory.path()),
            Err(UpdateError::Package("archive contains a special file"))
        );
        // The tar writer refuses to produce a traversing path, so the escaping
        // archive is assembled from raw headers, the way an attacker would.
        for name in ["root/../escape", "/etc/passwd"] {
            let escaping = gzip(&raw_tar(name, b"x"));
            assert!(
                extract(&escaping, ArchiveKind::TarGz, directory.path()).is_err(),
                "{name}"
            );
        }
        assert!(!directory.path().parent().unwrap().join("escape").exists());
    }

    #[test]
    fn oversized_archives_are_refused() {
        let mut budget = Budget::default();
        assert!(budget.file(MAX_ENTRY_BYTES + 1).is_err());
        let mut budget = Budget::default();
        for _ in 0..MAX_ENTRIES {
            budget.entry().unwrap();
        }
        assert!(budget.entry().is_err());
    }
}
