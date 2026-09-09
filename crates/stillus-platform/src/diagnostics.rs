// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! Fixed-vocabulary diagnostics for native test kits. Never format an error payload.

use std::io;
use std::path::Path;

#[derive(Clone, Copy, Debug)]
pub enum PathOperation {
    WorkspaceNote,
    ExternalSelection,
}

/// Diagnose representation mismatches without exposing either path.
pub fn path_comparison(_operation: PathOperation, _requested: &Path, _stored: &Path) {
    #[cfg(any(test, feature = "test-utils"))]
    {
        let canonical_equal = _requested
            .canonicalize()
            .ok()
            .zip(_stored.canonicalize().ok())
            .is_some_and(|(requested, stored)| requested == stored);
        let verbatim = |path: &Path| {
            #[cfg(windows)]
            if let Some(std::path::Component::Prefix(prefix)) = path.components().next() {
                return prefix.kind().is_verbatim();
            }
            let _ = path;
            false
        };
        eprintln!(
            "NATIVE_PATH operation={_operation:?} requested_verbatim={} stored_verbatim={} lexical_equal={} canonical_equal={canonical_equal}",
            verbatim(_requested),
            verbatim(_stored),
            _requested == _stored
        );
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Operation {
    Lock,
    Metadata,
    Replace,
    Cleanup,
}

#[derive(Clone, Copy, Debug)]
pub enum Stage {
    Create,
    Validate,
    Open,
    Acquire,
    Inspect,
    Permissions,
    Hash,
    Restore,
    Publish,
    Remove,
}

/// Retain the original error and report only operation/stage, kind and OS code.
/// Release applications compile this to a pass-through without diagnostic output.
pub fn io_result<T>(_operation: Operation, _stage: Stage, result: io::Result<T>) -> io::Result<T> {
    #[cfg(any(test, feature = "test-utils"))]
    if let Err(error) = &result {
        let kind = match error.kind() {
            io::ErrorKind::NotFound => "NotFound",
            io::ErrorKind::PermissionDenied => "PermissionDenied",
            io::ErrorKind::AlreadyExists => "AlreadyExists",
            io::ErrorKind::InvalidInput => "InvalidInput",
            io::ErrorKind::InvalidData => "InvalidData",
            io::ErrorKind::Unsupported => "Unsupported",
            io::ErrorKind::WouldBlock => "WouldBlock",
            io::ErrorKind::Interrupted => "Interrupted",
            io::ErrorKind::UnexpectedEof => "UnexpectedEof",
            _ => "Other",
        };
        eprintln!(
            "NATIVE_IO operation={_operation:?} stage={_stage:?} kind={kind} os_error={}",
            error.raw_os_error().unwrap_or(0)
        );
    }
    result
}

/// Correlate namespace-barrier failures with their caller without exposing paths.
#[cfg(any(windows, test))]
pub(crate) fn directory_sync_result<T>(
    _stage: &'static str,
    result: io::Result<T>,
) -> io::Result<T> {
    #[cfg(any(test, feature = "test-utils"))]
    if let Err(error) = &result {
        let kind = match error.kind() {
            io::ErrorKind::NotFound => "NotFound",
            io::ErrorKind::PermissionDenied => "PermissionDenied",
            io::ErrorKind::AlreadyExists => "AlreadyExists",
            io::ErrorKind::InvalidInput => "InvalidInput",
            io::ErrorKind::InvalidData => "InvalidData",
            io::ErrorKind::Unsupported => "Unsupported",
            io::ErrorKind::WouldBlock => "WouldBlock",
            io::ErrorKind::Interrupted => "Interrupted",
            io::ErrorKind::UnexpectedEof => "UnexpectedEof",
            _ => "Other",
        };
        eprintln!(
            "NATIVE_DIRECTORY_SYNC thread={:?} stage={_stage} kind={kind} os_error={}",
            std::thread::current().id(),
            error.raw_os_error().unwrap_or(0),
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_sync_diagnostics_preserve_results_and_error_payloads() {
        assert_eq!(directory_sync_result("FileSync", Ok(42)).unwrap(), 42);
        let error = directory_sync_result::<()>("Publish", Err(io::Error::from_raw_os_error(32)))
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(32));
        let error =
            directory_sync_result::<()>("Create", Err(io::Error::other("SYNTHETIC_SECRET")))
                .unwrap_err();
        assert_eq!(error.to_string(), "SYNTHETIC_SECRET");
    }
}
