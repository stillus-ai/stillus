// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use std::io;
use std::time::Duration;

const DELAYS_MS: [u64; 4] = [10, 20, 40, 80];

fn replace_with<S: PartialEq>(
    mut snapshot: impl FnMut() -> io::Result<S>,
    mut publish: impl FnMut() -> io::Result<()>,
    mut wait: impl FnMut(Duration),
) -> io::Result<()> {
    let original = snapshot()?;
    for attempt in 0..=DELAYS_MS.len() {
        let error = match publish() {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        let Some(delay) = DELAYS_MS.get(attempt) else {
            return Err(error);
        };
        if !matches!(error.raw_os_error(), Some(5 | 32))
            || snapshot().ok().as_ref() != Some(&original)
        {
            return Err(error);
        }
        wait(Duration::from_millis(*delay));
        if snapshot().ok().as_ref() != Some(&original) {
            return Err(error);
        }
    }
    unreachable!("the final publication returns its error")
}

#[cfg(windows)]
#[derive(PartialEq)]
struct Stamp {
    identity: crate::FileIdentity,
    links: u64,
    length: u64,
    modified: std::time::SystemTime,
    permissions: crate::fs::Permissions,
    digest: [u8; 32],
}

#[cfg(windows)]
fn stamp(path: &std::path::Path) -> io::Result<Option<Stamp>> {
    let metadata = match crate::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() {
        return Err(io::Error::other("replacement requires a regular file"));
    }
    Ok(Some(Stamp {
        identity: metadata.identity(),
        links: metadata.nlink(),
        length: metadata.len(),
        modified: metadata.modified()?,
        permissions: metadata.permissions(),
        digest: metadata.digest(),
    }))
}

#[cfg(windows)]
pub(super) fn replace(source: &std::path::Path, destination: &std::path::Path) -> io::Result<()> {
    replace_with(
        || {
            let source = stamp(source)?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "prepared replacement disappeared")
            })?;
            if source.links != 1 {
                return Err(io::Error::other("prepared replacement is linked"));
            }
            Ok((source, stamp(destination)?))
        },
        || crate::replace(source, destination),
        std::thread::sleep,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn transient_errors_retry_unchanged_files_with_a_fixed_bound() {
        let mut calls = 0;
        let mut delays = Vec::new();
        replace_with(
            || Ok((1, Some(2))),
            || {
                calls += 1;
                match calls {
                    1 => Err(io::Error::from_raw_os_error(32)),
                    2 => Err(io::Error::from_raw_os_error(5)),
                    _ => Ok(()),
                }
            },
            |delay| delays.push(delay),
        )
        .unwrap();
        assert_eq!(calls, 3);
        assert_eq!(
            delays,
            [Duration::from_millis(10), Duration::from_millis(20)]
        );
        for code in [5, 32, 87] {
            let mut calls = 0;
            let mut waits = 0;
            assert_eq!(
                replace_with(
                    || Ok(1),
                    || {
                        calls += 1;
                        Err(io::Error::from_raw_os_error(code))
                    },
                    |_| waits += 1
                )
                .unwrap_err()
                .raw_os_error(),
                Some(code)
            );
            assert_eq!(calls, if code == 87 { 1 } else { 5 });
            assert_eq!(waits, calls - 1);
        }
    }

    #[test]
    fn any_path_change_during_wait_prevents_another_publication() {
        for (initial, next) in [
            ((1, Some(3)), (2, Some(3))),
            ((1, Some(3)), (1, Some(4))),
            ((1, Some(3)), (1, None)),
            ((1, None), (1, Some(3))),
        ] {
            let state = Cell::new(initial);
            let mut calls = 0;
            let result = replace_with(
                || Ok(state.get()),
                || {
                    calls += 1;
                    Err(io::Error::from_raw_os_error(5))
                },
                |_| state.set(next),
            );
            assert_eq!(result.unwrap_err().raw_os_error(), Some(5));
            assert_eq!(calls, 1);
            assert_eq!(state.get(), next);
        }
    }

    #[test]
    fn missing_source_after_reported_failure_never_repeats_publication() {
        let published = Cell::new(false);
        let mut calls = 0;
        let result = replace_with(
            || {
                if published.get() {
                    Err(io::Error::from(io::ErrorKind::NotFound))
                } else {
                    Ok(1)
                }
            },
            || {
                calls += 1;
                published.set(true);
                Err(io::Error::from_raw_os_error(32))
            },
            |_| panic!("an uncertain publication must not be retried"),
        );
        assert_eq!(result.unwrap_err().raw_os_error(), Some(32));
        assert_eq!(calls, 1);
    }

    #[test]
    fn unreadable_initial_state_never_publishes() {
        let result = replace_with::<()>(
            || Err(io::Error::from(io::ErrorKind::PermissionDenied)),
            || panic!("an unverified replacement must not be published"),
            |_| panic!("there is no original state to revalidate"),
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    #[cfg(windows)]
    #[test]
    fn revalidation_allows_publication_after_a_delete_sharing_blocker_closes() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = std::env::temp_dir().join(format!(
            "stillus-revalidated-replace-{}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        let source = root.join("prepared");
        let target = root.join("target");
        std::fs::write(&source, b"new").unwrap();
        std::fs::write(&target, b"old").unwrap();
        let mut blocker = Some(
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(3)
                .open(&target)
                .unwrap(),
        );
        let mut attempts = 0;
        replace_with(
            || Ok((stamp(&source)?, stamp(&target)?)),
            || {
                attempts += 1;
                crate::replace(&source, &target)
            },
            |_| drop(blocker.take()),
        )
        .unwrap();
        assert!(attempts >= 2);
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert!(!source.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn stamps_detect_rewritten_bytes_even_with_restored_size_and_mtime() {
        use std::io::Write;
        let path =
            std::env::temp_dir().join(format!("stillus-replace-stamp-{}", std::process::id()));
        std::fs::write(&path, b"before").unwrap();
        let original = stamp(&path).unwrap().unwrap();
        let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.write_all(b"after!").unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(original.modified))
            .unwrap();
        drop(file);
        assert!(stamp(&path).unwrap().unwrap() != original);
        std::fs::remove_file(path).unwrap();
    }
}
