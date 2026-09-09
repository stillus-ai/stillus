// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! Retry only an unpublished Windows replacement, under the caller's operation lock.

use std::io;
use std::path::Path;
use std::time::Duration;

use super::{FileVersion, SaveError, SaveStage, TempGuard, fs, post_replace_failure, precommit};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

const RETRY_DELAYS_MS: [u64; 4] = [10, 20, 40, 80];

#[cfg(windows)]
pub(super) fn replace_note(
    guard: &mut TempGuard,
    path: &Path,
    expected: &FileVersion,
    prepared: &fs::Metadata,
) -> Result<(), SaveError> {
    replace_with(
        guard,
        path,
        expected,
        prepared,
        stillus_platform::replace,
        std::thread::sleep,
    )
}

fn replace_with(
    guard: &mut TempGuard,
    path: &Path,
    expected: &FileVersion,
    prepared: &fs::Metadata,
    mut publish: impl FnMut(&Path, &Path) -> io::Result<()>,
    mut wait: impl FnMut(Duration),
) -> Result<(), SaveError> {
    for attempt in 0..=RETRY_DELAYS_MS.len() {
        // Revalidate after every wait. Never publish over a newer external edit.
        if !validate_temp(guard, prepared)? {
            return Err(SaveError::InvalidTarget(
                "prepared replacement disappeared before publication".to_owned(),
            ));
        }
        validate_target(&inspect_target(path)?, expected)?;
        let error = match publish(guard.path(), path) {
            Ok(()) => {
                guard.disarm();
                return Ok(());
            }
            Err(error) => error,
        };
        // A reported failure must not lead to a second write if publication
        // nevertheless happened. Keep the original error and report uncertainty.
        let target = match inspect_target(path) {
            Ok(target) => target,
            Err(inspect_error) => {
                if !validate_temp(guard, prepared)? {
                    return Err(post_replace_failure("ReplaceReportedFailure", Some(error)));
                }
                return Err(inspect_error);
            }
        };
        if FileVersion::from_metadata(&target).same_file_as(&FileVersion::from_metadata(prepared)) {
            guard.disarm();
            return Err(post_replace_failure("ReplaceReportedFailure", Some(error)));
        }
        if !validate_temp(guard, prepared)? {
            return Err(post_replace_failure("ReplaceReportedFailure", Some(error)));
        }
        validate_target(&target, expected)?;
        if !matches!(error.raw_os_error(), Some(5 | 32)) {
            return Err(precommit(SaveStage::Replace, error));
        }
        let Some(delay_ms) = RETRY_DELAYS_MS.get(attempt) else {
            return Err(precommit(SaveStage::Replace, error));
        };
        #[cfg(any(test, feature = "test-utils"))]
        eprintln!(
            "NATIVE_REPLACE_RETRY thread={:?} attempt={} delay_ms={delay_ms} os_error={}",
            std::thread::current().id(),
            attempt + 1,
            error.raw_os_error().unwrap_or(0),
        );
        wait(Duration::from_millis(*delay_ms));
    }
    unreachable!("the last replacement attempt returns its error")
}

fn inspect_target(path: &Path) -> Result<fs::Metadata, SaveError> {
    fs::symlink_metadata(path).map_err(|error| precommit(SaveStage::ConflictCheck, error))
}

fn validate_target(metadata: &fs::Metadata, expected: &FileVersion) -> Result<(), SaveError> {
    let actual = FileVersion::from_metadata(metadata);
    if !metadata.file_type().is_file() || actual != *expected {
        return Err(super::version_conflict(
            "RewriteRetryTarget",
            &actual,
            expected,
        ));
    }
    Ok(())
}

/// False means the source disappeared and publication can no longer be retried.
fn validate_temp(guard: &mut TempGuard, prepared: &fs::Metadata) -> Result<bool, SaveError> {
    let metadata = match fs::symlink_metadata(guard.path()) {
        Ok(metadata) => metadata,
        Err(error) => {
            // Ownership cannot be verified; do not remove a replacement at this path.
            guard.disarm();
            return if error.kind() == io::ErrorKind::NotFound {
                Ok(false)
            } else {
                Err(precommit(SaveStage::ConflictCheck, error))
            };
        }
    };
    let actual = FileVersion::from_metadata(&metadata);
    let original = FileVersion::from_metadata(prepared);
    // Windows can finalize mtime when the writer closes. Identity, size and the
    // handle-computed digest must still match the fully written, flushed file.
    #[cfg(windows)]
    let content_matches = actual.digest == original.digest;
    #[cfg(not(windows))]
    let content_matches = actual == original;
    if !metadata.file_type().is_file()
        || metadata.nlink() != 1
        || !actual.same_file_as(&original)
        || actual.size != original.size
        || !content_matches
        || metadata.permissions() != prepared.permissions()
    {
        guard.disarm();
        return Err(SaveError::InvalidTarget(
            "prepared replacement changed before publication".to_owned(),
        ));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    const ORIGINAL: &[u8] = b"original\n";
    const REPLACEMENT: &[u8] = b"replacement\n";

    struct Fixture {
        root: PathBuf,
        target: PathBuf,
        source: PathBuf,
        expected: FileVersion,
        prepared: fs::Metadata,
    }

    impl Fixture {
        fn new() -> (Self, TempGuard) {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "stillus-replace-retry-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            fs::create_dir(&root).unwrap();
            let target = root.join("note.md");
            fs::write(&target, ORIGINAL).unwrap();
            let (_, expected) = super::super::open_versioned(&target).unwrap();
            let (guard, mut file) = super::super::create_temp(&target, &root).unwrap();
            file.write_all(REPLACEMENT).unwrap();
            file.sync_all().unwrap();
            let prepared = file.metadata().unwrap();
            drop(file);
            let source = guard.path().to_path_buf();
            (
                Self {
                    root,
                    target,
                    source,
                    expected,
                    prepared,
                },
                guard,
            )
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if !std::thread::panicking() {
                fs::remove_dir_all(&self.root).unwrap();
            }
        }
    }

    #[test]
    fn transient_failures_publish_prepared_bytes_once() {
        let (fixture, mut guard) = Fixture::new();
        let mut calls = 0;
        let mut waits = Vec::new();
        replace_with(
            &mut guard,
            &fixture.target,
            &fixture.expected,
            &fixture.prepared,
            |source, target| {
                calls += 1;
                match calls {
                    1 => Err(io::Error::from_raw_os_error(5)),
                    2 => Err(io::Error::from_raw_os_error(32)),
                    _ => fs::rename(source, target),
                }
            },
            |delay| waits.push(delay),
        )
        .unwrap();
        drop(guard);
        assert_eq!(calls, 3);
        assert_eq!(
            waits,
            [Duration::from_millis(10), Duration::from_millis(20)]
        );
        assert_eq!(fs::read(&fixture.target).unwrap(), REPLACEMENT);
        assert!(!fixture.source.exists());
    }

    #[test]
    fn persistent_and_unrelated_errors_preserve_target_and_cleanup_owned_temp() {
        for code in [Some(5), Some(32), Some(87), None] {
            let (fixture, mut guard) = Fixture::new();
            let error = || {
                code.map_or_else(
                    || io::Error::new(io::ErrorKind::PermissionDenied, "permanent denial"),
                    io::Error::from_raw_os_error,
                )
            };
            let mut calls = 0;
            let mut waits = Vec::new();
            let result = replace_with(
                &mut guard,
                &fixture.target,
                &fixture.expected,
                &fixture.prepared,
                |_, _| {
                    calls += 1;
                    Err(error())
                },
                |delay| waits.push(delay),
            );
            assert_eq!(
                result,
                Err(SaveError::PreCommit {
                    stage: SaveStage::Replace,
                    message: error().to_string(),
                })
            );
            let retryable = matches!(code, Some(5 | 32));
            assert_eq!(calls, if retryable { 5 } else { 1 });
            let expected_waits: Vec<_> = if retryable {
                RETRY_DELAYS_MS
                    .into_iter()
                    .map(Duration::from_millis)
                    .collect()
            } else {
                Vec::new()
            };
            assert_eq!(waits, expected_waits);
            assert_eq!(fs::read(&fixture.target).unwrap(), ORIGINAL);
            drop(guard);
            assert!(!fixture.source.exists());
        }
    }

    #[test]
    fn external_edit_during_wait_is_a_conflict_without_another_publication() {
        let (fixture, mut guard) = Fixture::new();
        let mut calls = 0;
        let result = replace_with(
            &mut guard,
            &fixture.target,
            &fixture.expected,
            &fixture.prepared,
            |_, _| {
                calls += 1;
                Err(io::Error::from_raw_os_error(5))
            },
            |_| fs::write(&fixture.target, b"external edit\n").unwrap(),
        );
        assert_eq!(result, Err(SaveError::Conflict));
        assert_eq!(calls, 1);
        assert_eq!(fs::read(&fixture.target).unwrap(), b"external edit\n");
        drop(guard);
        assert!(!fixture.source.exists());
    }

    #[test]
    fn replaced_temp_is_neither_published_nor_removed() {
        let (fixture, mut guard) = Fixture::new();
        let foreign = fixture.root.join("foreign");
        fs::write(&foreign, b"foreign data\n").unwrap();
        let mut calls = 0;
        let result = replace_with(
            &mut guard,
            &fixture.target,
            &fixture.expected,
            &fixture.prepared,
            |_, _| {
                calls += 1;
                Err(io::Error::from_raw_os_error(32))
            },
            |_| fs::rename(&foreign, &fixture.source).unwrap(),
        );
        assert!(matches!(result, Err(SaveError::InvalidTarget(_))));
        assert_eq!(calls, 1);
        drop(guard);
        assert_eq!(fs::read(&fixture.source).unwrap(), b"foreign data\n");
        assert_eq!(fs::read(&fixture.target).unwrap(), ORIGINAL);
    }

    #[test]
    fn reported_failure_after_publication_never_repeats_or_removes_a_new_source() {
        let (fixture, mut guard) = Fixture::new();
        let result = replace_with(
            &mut guard,
            &fixture.target,
            &fixture.expected,
            &fixture.prepared,
            |source, target| {
                fs::rename(source, target)?;
                fs::write(source, b"new source owned by another writer\n")?;
                Err(io::Error::from_raw_os_error(5))
            },
            |_| panic!("must not retry an already published replacement"),
        );
        assert_eq!(
            result,
            Err(SaveError::PostReplaceSync {
                message: io::Error::from_raw_os_error(5).to_string(),
            })
        );
        drop(guard);
        assert_eq!(fs::read(&fixture.target).unwrap(), REPLACEMENT);
        assert_eq!(
            fs::read(&fixture.source).unwrap(),
            b"new source owned by another writer\n"
        );
    }

    #[cfg(windows)]
    #[test]
    fn temp_digest_detects_same_size_edits_with_restored_mtime() {
        let (fixture, mut guard) = Fixture::new();
        let modified = fs::symlink_metadata(&fixture.source)
            .unwrap()
            .modified()
            .unwrap();
        let result = replace_with(
            &mut guard,
            &fixture.target,
            &fixture.expected,
            &fixture.prepared,
            |_, _| Err(io::Error::from_raw_os_error(5)),
            |_| {
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&fixture.source)
                    .unwrap();
                file.write_all(b"XXXXXXXXXXX\n").unwrap();
                file.set_times(std::fs::FileTimes::new().set_modified(modified))
                    .unwrap();
            },
        );
        assert!(matches!(result, Err(SaveError::InvalidTarget(_))));
        drop(guard);
        assert_eq!(fs::read(&fixture.source).unwrap(), b"XXXXXXXXXXX\n");
        assert_eq!(fs::read(&fixture.target).unwrap(), ORIGINAL);
    }

    #[cfg(windows)]
    #[test]
    fn windows_replacement_succeeds_after_a_delete_sharing_blocker_closes() {
        use std::os::windows::fs::OpenOptionsExt;

        let (fixture, mut guard) = Fixture::new();
        let mut blocker = Some(
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(1 | 2)
                .open(&fixture.target)
                .unwrap(),
        );
        let mut calls = 0;
        replace_with(
            &mut guard,
            &fixture.target,
            &fixture.expected,
            &fixture.prepared,
            |source, target| {
                calls += 1;
                fs::rename(source, target)
            },
            |_| drop(blocker.take()),
        )
        .unwrap();
        assert!(calls >= 2);
        drop(guard);
        assert_eq!(fs::read(&fixture.target).unwrap(), REPLACEMENT);
        assert!(!fixture.source.exists());
    }
}
