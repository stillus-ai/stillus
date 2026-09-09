// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! Bounded retries for empty Windows namespace barriers, never for note writes.

use std::{fs::File, io, path::Path, time::Duration};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Ownership {
    Owned,
    Missing,
    Changed,
    Unavailable,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Stage {
    Publish,
    Remove,
}

/// Inspect the pathname through a handle without following a final reparse point.
/// The original handle stays open, so its identity cannot be recycled.
pub(crate) fn ownership(
    original: &File,
    path: &Path,
    open: impl FnOnce(&Path) -> io::Result<File>,
) -> Ownership {
    let current = match open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ownership::Missing,
        Err(_) => return Ownership::Unavailable,
    };
    let inspect = || {
        let original_info = crate::file_information(original)?;
        let current_info = crate::file_information(&current)?;
        let metadata = current.metadata()?;
        Ok::<_, io::Error>(
            original_info.identity == current_info.identity
                && original_info.links == 1
                && current_info.links == 1
                && metadata.is_file()
                && !crate::is_link(&metadata)
                && metadata.len() == 0,
        )
    };
    match inspect() {
        Ok(true) => Ownership::Owned,
        Ok(false) => Ownership::Changed,
        Err(_) => Ownership::Unavailable,
    }
}

#[derive(Default)]
pub(crate) struct RetryBudget {
    waits: usize,
}

impl RetryBudget {
    pub(crate) fn run(
        &mut self,
        stage: Stage,
        mut operation: impl FnMut() -> io::Result<()>,
        mut inspect: impl FnMut() -> Ownership,
        mut wait: impl FnMut(Duration),
    ) -> io::Result<()> {
        const DELAYS_MS: [u64; 6] = [10, 20, 40, 80, 160, 320];
        if inspect() != Ownership::Owned {
            return Err(io::Error::other(
                "namespace barrier ownership could not be verified",
            ));
        }
        loop {
            match operation() {
                Ok(()) => return Ok(()),
                Err(error) => {
                    if error.raw_os_error() != Some(32) {
                        return Err(error);
                    }
                    let owner = inspect();
                    let delay = DELAYS_MS.get(self.waits).copied();
                    // A missing/replaced source may mean MoveFileEx committed before
                    // returning an error. Preserve that error, never infer success.
                    if owner != Ownership::Owned || delay.is_none() {
                        diagnose(stage, self.waits + 1, 0, owner);
                        return Err(error);
                    }
                    let delay = delay.expect("checked retry budget");
                    diagnose(stage, self.waits + 1, delay, owner);
                    self.waits += 1;
                    wait(Duration::from_millis(delay));
                    // The pathname may have changed during the wait, even if
                    // it still belonged to this operation immediately before it.
                    let owner = inspect();
                    if owner != Ownership::Owned {
                        diagnose(stage, self.waits, 0, owner);
                        return Err(error);
                    }
                }
            }
        }
    }
}

fn diagnose(_stage: Stage, _attempt: usize, _delay: u64, _owner: Ownership) {
    #[cfg(any(test, feature = "test-utils"))]
    eprintln!(
        "NATIVE_DIRECTORY_SYNC_RETRY thread={:?} stage={_stage:?} attempt={_attempt} delay_ms={_delay} os_error=32 ownership={_owner:?}",
        std::thread::current().id(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, fs};

    #[test]
    fn transient_conflicts_retry_only_the_unfinished_marker_stage() {
        let mut budget = RetryBudget::default();
        let mut delays = Vec::new();
        let mut publish = 0;
        budget
            .run(
                Stage::Publish,
                || {
                    publish += 1;
                    if publish < 3 {
                        Err(io::Error::from_raw_os_error(32))
                    } else {
                        Ok(())
                    }
                },
                || Ownership::Owned,
                |delay| delays.push(delay.as_millis()),
            )
            .unwrap();
        let mut remove = 0;
        budget
            .run(
                Stage::Remove,
                || {
                    remove += 1;
                    if remove < 3 {
                        Err(io::Error::from_raw_os_error(32))
                    } else {
                        Ok(())
                    }
                },
                || Ownership::Owned,
                |delay| delays.push(delay.as_millis()),
            )
            .unwrap();
        assert_eq!(publish, 3);
        assert_eq!(remove, 3);
        assert_eq!(delays, [10, 20, 40, 80]);
    }

    #[test]
    fn permanent_conflicts_exhaust_one_shared_budget() {
        let mut budget = RetryBudget::default();
        let mut delays = Vec::new();
        let mut calls = 0;
        let error = budget
            .run(
                Stage::Publish,
                || {
                    calls += 1;
                    Err(io::Error::from_raw_os_error(32))
                },
                || Ownership::Owned,
                |delay| delays.push(delay.as_millis()),
            )
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(32));
        assert_eq!(calls, 7);
        assert_eq!(delays, [10, 20, 40, 80, 160, 320]);
        assert_eq!(delays.iter().sum::<u128>(), 630);
        budget
            .run(
                Stage::Remove,
                || Err(io::Error::from_raw_os_error(32)),
                || Ownership::Owned,
                |_| panic!("budget must not reset"),
            )
            .unwrap_err();
    }

    #[test]
    fn other_errors_never_retry() {
        for code in [5, 80, 112, 183, 1117] {
            let error = RetryBudget::default()
                .run(
                    Stage::Publish,
                    || Err(io::Error::from_raw_os_error(code)),
                    || Ownership::Owned,
                    |_| panic!("non-sharing error retried"),
                )
                .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(code));
        }
    }

    #[test]
    fn ownership_loss_before_or_during_wait_preserves_the_original_error() {
        for lost in [
            Ownership::Missing,
            Ownership::Changed,
            Ownership::Unavailable,
        ] {
            for during_wait in [false, true] {
                let owner = Cell::new(Ownership::Owned);
                let calls = Cell::new(0);
                let error = RetryBudget::default()
                    .run(
                        Stage::Publish,
                        || {
                            calls.set(calls.get() + 1);
                            if !during_wait {
                                owner.set(lost);
                            }
                            Err(io::Error::from_raw_os_error(32))
                        },
                        || owner.get(),
                        |_| owner.set(lost),
                    )
                    .unwrap_err();
                assert_eq!(error.raw_os_error(), Some(32));
                assert_eq!(calls.get(), 1);
            }
        }
    }

    #[test]
    fn inspection_checks_identity_empty_regular_file_and_link_count() {
        let directory = crate::tests::TestDirectory::new();
        let path = directory.0.join("marker");
        let file = File::create_new(&path).unwrap();
        let check = || ownership(&file, &path, |path| File::open(path));
        assert_eq!(check(), Ownership::Owned);
        fs::hard_link(&path, directory.0.join("link")).unwrap();
        assert_eq!(check(), Ownership::Changed);
        fs::remove_file(directory.0.join("link")).unwrap();
        fs::write(&path, b"foreign data").unwrap();
        assert_eq!(check(), Ownership::Changed);
        fs::rename(&path, directory.0.join("moved")).unwrap();
        assert_eq!(check(), Ownership::Missing);
        fs::write(&path, b"").unwrap();
        assert_eq!(check(), Ownership::Changed);
        assert_eq!(
            ownership(&file, &path, |_| Err(io::Error::from_raw_os_error(5))),
            Ownership::Unavailable
        );
    }

    #[test]
    fn error_after_actual_rename_never_reports_success_or_retries() {
        let directory = crate::tests::TestDirectory::new();
        let source = directory.0.join("marker");
        let destination = directory.0.join("done");
        let note = directory.0.join("note.md");
        fs::write(&note, b"committed note").unwrap();
        let file = File::create_new(&source).unwrap();
        let mut calls = 0;
        let error = RetryBudget::default()
            .run(
                Stage::Publish,
                || {
                    calls += 1;
                    fs::rename(&source, &destination).unwrap();
                    Err(io::Error::from_raw_os_error(32))
                },
                || ownership(&file, &source, |path| File::open(path)),
                |_| panic!("uncertain publish retried"),
            )
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(32));
        assert_eq!(calls, 1);
        assert!(destination.exists());
        assert_eq!(fs::read(note).unwrap(), b"committed note");
    }
}
