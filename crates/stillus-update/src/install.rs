// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! Locating the installed application and replacing it in place.
//!
//! macOS bundles are atomically exchanged with the staged bundle, keeping the
//! installed path present even if the process exits during publication. Other
//! packages use a sequence of renames with rollback on error. The caller offers
//! an explicit restart afterwards; installation never starts a process or
//! closes the running application.

use crate::UpdateError;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const BUNDLE_NAME: &str = "Stillus.app";
const MAC_EXECUTABLE: &str = "Contents/MacOS/Stillus";
const MAC_MANIFEST: &str = "Contents/Resources/release.json";
const LINUX_EXECUTABLE: &str = "stillus";
const WINDOWS_EXECUTABLE: &str = "Stillus.exe";
const WINDOWS_MANIFEST: &str = "dependencies.json";
const BUILD_MANIFEST: &str = "build.json";
const STAGING_PREFIX: &str = ".stillus-update-";
const MAX_STAGED_FILES: usize = 20_000;

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstallKind {
    /// A macOS application bundle, replaced as a whole directory.
    MacApp,
    /// A Linux package directory holding the `stillus` executable.
    Linux,
    /// A Windows package directory holding `Stillus.exe` and its libraries.
    Windows,
}

impl InstallKind {
    /// Platform name used by the package manifest.
    pub(crate) fn platform(self) -> &'static str {
        match self {
            Self::MacApp => "macos",
            Self::Linux => "linux",
            Self::Windows => "windows",
        }
    }
}

/// A packaged installation that can be replaced in place.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Installation {
    kind: InstallKind,
    root: PathBuf,
    target: PathBuf,
}

impl Installation {
    /// Recognizes the installation that contains the running executable.
    pub fn locate() -> Result<Self, UpdateError> {
        let executable = std::env::current_exe().map_err(|_| UpdateError::NotInstalled)?;
        Self::detect(&executable)
    }

    /// Recognizes the installation layout around an executable path.
    pub fn detect(executable: &Path) -> Result<Self, UpdateError> {
        let executable = executable
            .canonicalize()
            .map_err(|_| UpdateError::NotInstalled)?;
        if cfg!(target_os = "macos") {
            let bundle = executable
                .ancestors()
                .find(|path| path.extension().is_some_and(|value| value == "app"))
                .ok_or(UpdateError::NotInstalled)?;
            Self::mac_app(bundle.to_path_buf())
        } else if cfg!(windows) {
            Self::windows(executable)
        } else {
            Self::linux(executable)
        }
    }

    /// A macOS bundle, identified by the layout the packager produces.
    pub fn mac_app(bundle: PathBuf) -> Result<Self, UpdateError> {
        let root = parent(&bundle)?;
        if bundle.extension().is_none_or(|value| value != "app")
            || !bundle.join(MAC_EXECUTABLE).is_file()
            || !bundle.join(MAC_MANIFEST).is_file()
        {
            return Err(UpdateError::NotInstalled);
        }
        Ok(Self {
            kind: InstallKind::MacApp,
            root,
            target: bundle,
        })
    }

    /// A Linux package directory, identified by the packaged executable name
    /// and the build manifest that ships beside it. A development build never
    /// matches, so `cargo run` cannot replace a checkout.
    pub fn linux(executable: PathBuf) -> Result<Self, UpdateError> {
        let root = parent(&executable)?;
        if executable
            .file_name()
            .is_none_or(|name| name != LINUX_EXECUTABLE)
            || !root.join(BUILD_MANIFEST).is_file()
        {
            return Err(UpdateError::NotInstalled);
        }
        Ok(Self {
            kind: InstallKind::Linux,
            root,
            target: executable,
        })
    }

    /// A Windows package directory, identified by the packaged executable name
    /// and the dependency manifest that ships beside it.
    pub fn windows(executable: PathBuf) -> Result<Self, UpdateError> {
        let root = parent(&executable)?;
        let named = executable
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case(WINDOWS_EXECUTABLE));
        if !named || !root.join(WINDOWS_MANIFEST).is_file() {
            return Err(UpdateError::NotInstalled);
        }
        Ok(Self {
            kind: InstallKind::Windows,
            root,
            target: executable,
        })
    }

    pub fn kind(&self) -> InstallKind {
        self.kind
    }

    /// Directory whose contents the update replaces.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The bundle or executable that carries the running version.
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// Fails before anything is downloaded when the installation belongs to
    /// another user, for example an application installed by an administrator.
    pub fn ensure_writable(&self) -> Result<(), UpdateError> {
        let probe = unique(&self.root, "probe");
        match fs::File::create(&probe) {
            Ok(file) => {
                drop(file);
                let _ = fs::remove_file(&probe);
                Ok(())
            }
            Err(_) => Err(UpdateError::ReadOnly),
        }
    }

    /// A staging directory on the same filesystem as the installation, so the
    /// final replacement is a rename and never a copy.
    pub(crate) fn staging(&self) -> Result<Staging, UpdateError> {
        // Hold the OS lock through extraction, validation, replacement/rollback
        // and Staging::drop. A second updater waits; startup cleanup skips it.
        let operation = stillus_platform::OperationLock::directory(&self.root)?;
        let path = unique(&self.root, "staging");
        fs::create_dir(&path)?;
        Ok(Staging {
            path,
            _operation: operation,
        })
    }

    /// Moves an extracted package into place.
    pub(crate) fn apply(&self, staged: &Staging) -> Result<(), UpdateError> {
        if staged.path().parent() != Some(self.root()) {
            return Err(UpdateError::Package(
                "staging belongs to another installation",
            ));
        }
        match self.kind {
            InstallKind::MacApp => self.apply_bundle(staged.path()),
            InstallKind::Linux | InstallKind::Windows => self.apply_files(staged.path()),
        }
    }

    fn apply_bundle(&self, staged: &Path) -> Result<(), UpdateError> {
        self.apply_bundle_with_exchange(staged, exchange_directories)
    }

    fn apply_bundle_with_exchange(
        &self,
        staged: &Path,
        exchange: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
    ) -> Result<(), UpdateError> {
        let source = staged.join(BUNDLE_NAME);
        if !source.join(MAC_EXECUTABLE).is_file() {
            return Err(UpdateError::Package("package has no application bundle"));
        }
        // Never fall back to moving the installed bundle aside: a process exit
        // between two renames would leave no application to launch for recovery.
        // After exchange the old bundle stays in staging until Staging::drop,
        // or startup cleanup if this process exits before dropping the guard.
        exchange(&source, &self.target)?;
        stillus_platform::sync_directory(&self.root)?;
        stillus_platform::sync_directory(staged)?;
        Ok(())
    }

    fn apply_files(&self, staged: &Path) -> Result<(), UpdateError> {
        let mut relative = Vec::new();
        collect(staged, Path::new(""), &mut relative)?;
        if relative.iter().any(|path| {
            path.components().next().is_some_and(|component| {
                component
                    .as_os_str()
                    .to_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case(".stillus-operation.lock"))
            })
        }) {
            return Err(UpdateError::Package(
                "package replaces the installation lock",
            ));
        }
        relative.sort();
        let mut retired: Vec<(PathBuf, PathBuf)> = Vec::new();
        let mut installed: Vec<PathBuf> = Vec::new();
        for path in &relative {
            let destination = self.root.join(path);
            if let Some(parent) = destination.parent()
                && let Err(error) = fs::create_dir_all(parent)
            {
                return rollback(retired, installed, error);
            }
            if destination.symlink_metadata().is_ok() {
                let previous = unique(&self.root, "file");
                if let Err(error) = fs::rename(&destination, &previous) {
                    return rollback(retired, installed, error);
                }
                retired.push((previous, destination.clone()));
            }
            if let Err(error) = fs::rename(staged.join(path), &destination) {
                return rollback(retired, installed, error);
            }
            installed.push(destination);
        }
        for (previous, _) in retired {
            // The running executable cannot be deleted on every platform;
            // whatever survives is removed by the next start.
            let _ = fs::remove_file(&previous);
        }
        Ok(())
    }

    /// Removes replaced files and abandoned staging directories. Safe to call
    /// at any time: an active update is skipped without blocking startup.
    /// Failures are ignored because the next start retries.
    pub fn cleanup(&self) {
        let Ok(Some(_operation)) = stillus_platform::OperationLock::try_directory(&self.root)
        else {
            return;
        };
        let Ok(entries) = fs::read_dir(&self.root) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.starts_with(STAGING_PREFIX) {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                let _ = fs::remove_dir_all(&path);
            } else {
                let _ = fs::remove_file(&path);
            }
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn exchange_directories(source: &Path, destination: &Path) -> std::io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    // rustix maps EXCHANGE to RENAME_SWAP on macOS and RENAME_EXCHANGE on
    // Linux, where the same bundle-publication tests run in the toolchain.
    renameat_with(CWD, source, CWD, destination, RenameFlags::EXCHANGE)?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn exchange_directories(_source: &Path, _destination: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic application bundle exchange is unavailable",
    ))
}

fn rollback(
    retired: Vec<(PathBuf, PathBuf)>,
    installed: Vec<PathBuf>,
    error: std::io::Error,
) -> Result<(), UpdateError> {
    for path in installed {
        let _ = fs::remove_file(&path);
    }
    for (previous, destination) in retired {
        let _ = fs::rename(&previous, &destination);
    }
    Err(UpdateError::Io(error.to_string()))
}

fn parent(path: &Path) -> Result<PathBuf, UpdateError> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .ok_or(UpdateError::NotInstalled)
}

/// Collects the relative paths of every regular file below `root`.
fn collect(root: &Path, prefix: &Path, result: &mut Vec<PathBuf>) -> Result<(), UpdateError> {
    for entry in fs::read_dir(root.join(prefix))? {
        let entry = entry?;
        let relative = prefix.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect(root, &relative, result)?;
        } else if kind.is_file() {
            result.push(relative);
            if result.len() > MAX_STAGED_FILES {
                return Err(UpdateError::Package("package has too many files"));
            }
        } else {
            return Err(UpdateError::Package("package contains a special file"));
        }
    }
    Ok(())
}

/// A hidden name inside the installation directory that no package uses.
/// Every temporary name shares one prefix so that `cleanup` recognizes it.
fn unique(root: &Path, purpose: &str) -> PathBuf {
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    root.join(format!("{STAGING_PREFIX}{purpose}-{stamp}-{sequence}"))
}

/// Removes its directory when the update finishes or fails.
pub(crate) struct Staging {
    path: PathBuf,
    _operation: stillus_platform::OperationLock,
}

impl Staging {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn mac_layout(root: &Path) -> PathBuf {
        let bundle = root.join(BUNDLE_NAME);
        write(&bundle.join(MAC_EXECUTABLE), "old binary");
        write(&bundle.join(MAC_MANIFEST), "{}");
        bundle
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn cleanup_preserves_active_staging_and_exchanged_bundle() {
        let directory = tempfile::tempdir().unwrap();
        let bundle = mac_layout(directory.path());
        let installation = Installation::mac_app(bundle.clone()).unwrap();
        let staged = installation.staging().unwrap();
        write(
            &staged.path().join(BUNDLE_NAME).join(MAC_EXECUTABLE),
            "new binary",
        );
        installation.apply(&staged).unwrap();
        let retired = staged.path().join(BUNDLE_NAME);
        let cleaner = installation.clone();
        let (send, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            cleaner.cleanup();
            send.send(()).unwrap();
        });
        // Startup cleanup must also remain nonblocking while an update owns the lock.
        receive
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        worker.join().unwrap();
        assert!(
            staged
                .path()
                .join(BUNDLE_NAME)
                .join(MAC_EXECUTABLE)
                .is_file()
        );
        assert!(retired.join(MAC_EXECUTABLE).is_file());
        // Same-thread cleanup must not bypass the guard either.
        installation.cleanup();
        assert!(retired.exists());
        drop(staged);
        installation.cleanup();
        assert!(!retired.exists());
        assert_eq!(
            fs::read_to_string(bundle.join(MAC_EXECUTABLE)).unwrap(),
            "new binary"
        );
    }

    #[test]
    fn packages_cannot_replace_the_installation_lock() {
        for name in [
            ".stillus-operation.lock",
            ".STILLUS-OPERATION.LOCK",
            ".stillus-operation.lock/nested",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            write(&root.join("stillus"), "old binary");
            write(&root.join("build.json"), "{}");
            let installation = Installation::linux(root.join("stillus")).unwrap();
            let staging = installation.staging().unwrap();
            write(&staging.path().join(name), "package data");
            write(&staging.path().join("stillus"), "new binary");
            assert_eq!(
                installation.apply(&staging),
                Err(UpdateError::Package(
                    "package replaces the installation lock"
                ))
            );
            assert_eq!(fs::read(root.join(".stillus-operation.lock")).unwrap(), b"");
            assert_eq!(
                fs::read_to_string(root.join("stillus")).unwrap(),
                "old binary"
            );
        }
    }

    #[test]
    fn concurrent_staging_is_serialized_until_the_first_guard_drops() {
        let directory = tempfile::tempdir().unwrap();
        let installation = Installation::mac_app(mac_layout(directory.path())).unwrap();
        let first = installation.staging().unwrap();
        let (entered_send, entered_receive) = std::sync::mpsc::channel();
        let (ready_send, ready_receive) = std::sync::mpsc::channel();
        let other = installation.clone();
        let worker = std::thread::spawn(move || {
            ready_send.send(()).unwrap();
            let second = other.staging().unwrap();
            entered_send.send(second.path().to_path_buf()).unwrap();
        });
        ready_receive
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let blocked = entered_receive
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_err();
        let first_path = first.path().to_owned();
        drop(first);
        if blocked {
            entered_receive
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
        }
        worker.join().unwrap();
        assert!(
            blocked,
            "another installation entered while the first was active"
        );
        assert!(!first_path.exists());
        installation.cleanup();
    }

    #[test]
    fn packaged_layouts_are_recognized_and_checkouts_are_not() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let bundle = mac_layout(root);
        let installation = Installation::mac_app(bundle.clone()).unwrap();
        assert_eq!(installation.kind(), InstallKind::MacApp);
        assert_eq!(installation.root(), root);
        assert_eq!(installation.target(), bundle);

        write(&root.join("linux/stillus"), "binary");
        assert_eq!(
            Installation::linux(root.join("linux/stillus")),
            Err(UpdateError::NotInstalled)
        );
        write(&root.join("linux/build.json"), "{}");
        assert_eq!(
            Installation::linux(root.join("linux/stillus"))
                .unwrap()
                .root(),
            root.join("linux")
        );
        write(&root.join("linux/stillus-app"), "development build");
        assert_eq!(
            Installation::linux(root.join("linux/stillus-app")),
            Err(UpdateError::NotInstalled)
        );

        write(&root.join("windows/Stillus.exe"), "binary");
        assert_eq!(
            Installation::windows(root.join("windows/Stillus.exe")),
            Err(UpdateError::NotInstalled)
        );
        write(&root.join("windows/dependencies.json"), "{}");
        assert!(Installation::windows(root.join("windows/Stillus.exe")).is_ok());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn bundles_are_exchanged_and_invalid_packages_preserve_the_installation() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let bundle = mac_layout(root);
        let installation = Installation::mac_app(bundle.clone()).unwrap();
        installation.ensure_writable().unwrap();

        let staged = installation.staging().unwrap();
        write(
            &staged.path().join(BUNDLE_NAME).join(MAC_EXECUTABLE),
            "new binary",
        );
        write(
            &staged.path().join(BUNDLE_NAME).join(MAC_MANIFEST),
            "new manifest",
        );
        installation.apply(&staged).unwrap();
        assert_eq!(
            fs::read_to_string(bundle.join(MAC_EXECUTABLE)).unwrap(),
            "new binary"
        );
        assert_eq!(
            fs::read_to_string(bundle.join(MAC_MANIFEST)).unwrap(),
            "new manifest"
        );
        assert_eq!(
            fs::read_to_string(staged.path().join(BUNDLE_NAME).join(MAC_EXECUTABLE)).unwrap(),
            "old binary"
        );
        assert_eq!(
            fs::read_to_string(staged.path().join(BUNDLE_NAME).join(MAC_MANIFEST)).unwrap(),
            "{}"
        );
        drop(staged);

        let empty = installation.staging().unwrap();
        assert_eq!(
            installation.apply(&empty),
            Err(UpdateError::Package("package has no application bundle"))
        );
        assert_eq!(
            fs::read_to_string(bundle.join(MAC_EXECUTABLE)).unwrap(),
            "new binary"
        );
    }

    #[test]
    fn failed_bundle_exchange_never_moves_or_deletes_the_installed_bundle() {
        for kind in [
            std::io::ErrorKind::Unsupported,
            std::io::ErrorKind::PermissionDenied,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let bundle = mac_layout(directory.path());
            let installation = Installation::mac_app(bundle.clone()).unwrap();
            let staged = installation.staging().unwrap();
            let candidate = staged.path().join(BUNDLE_NAME);
            write(&candidate.join(MAC_EXECUTABLE), "new binary");
            let result =
                installation.apply_bundle_with_exchange(staged.path(), |source, target| {
                    assert_eq!(source, candidate);
                    assert_eq!(target, bundle);
                    assert_eq!(
                        fs::read_to_string(target.join(MAC_EXECUTABLE)).unwrap(),
                        "old binary"
                    );
                    Err(std::io::Error::new(kind, "exchange refused"))
                });
            assert_eq!(result, Err(UpdateError::Io("exchange refused".into())));
            assert_eq!(
                fs::read_to_string(candidate.join(MAC_EXECUTABLE)).unwrap(),
                "new binary"
            );
            drop(staged);
            installation.cleanup();
            assert_eq!(
                fs::read_to_string(bundle.join(MAC_EXECUTABLE)).unwrap(),
                "old binary"
            );
            assert!(Installation::mac_app(bundle).is_ok());
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn abandoned_bundle_staging_keeps_a_complete_installation_before_and_after_exchange() {
        for published in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let bundle = mac_layout(directory.path());
            let installation = Installation::mac_app(bundle.clone()).unwrap();
            // No Staging guard: model an exit that skips destructors, on either
            // side of the atomic publication, leaving startup to clean staging.
            let staged = unique(directory.path(), "staging");
            write(&staged.join(BUNDLE_NAME).join(MAC_EXECUTABLE), "new binary");
            write(&staged.join(BUNDLE_NAME).join(MAC_MANIFEST), "new manifest");
            if published {
                installation.apply_bundle(&staged).unwrap();
            }
            let reopened = Installation::mac_app(bundle.clone()).unwrap();
            reopened.cleanup();
            assert!(!staged.exists());
            assert_eq!(
                fs::read_to_string(bundle.join(MAC_EXECUTABLE)).unwrap(),
                if published {
                    "new binary"
                } else {
                    "old binary"
                }
            );
            assert_eq!(
                fs::read_to_string(bundle.join(MAC_MANIFEST)).unwrap(),
                if published { "new manifest" } else { "{}" }
            );
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    #[test]
    fn unsupported_bundle_exchange_keeps_the_installed_bundle() {
        let directory = tempfile::tempdir().unwrap();
        let bundle = mac_layout(directory.path());
        let installation = Installation::mac_app(bundle.clone()).unwrap();
        let staged = installation.staging().unwrap();
        write(
            &staged.path().join(BUNDLE_NAME).join(MAC_EXECUTABLE),
            "new binary",
        );
        assert!(installation.apply(&staged).is_err());
        drop(staged);
        assert_eq!(
            fs::read_to_string(bundle.join(MAC_EXECUTABLE)).unwrap(),
            "old binary"
        );
    }

    #[test]
    fn files_are_replaced_and_leftovers_removed() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("linux");
        write(&root.join("stillus"), "old binary");
        write(&root.join("build.json"), "{\"platform\":\"linux\"}");
        write(&root.join("LICENSE.txt"), "old license");
        let installation = Installation::linux(root.join("stillus")).unwrap();

        let staged = installation.staging().unwrap();
        write(&staged.path().join("stillus"), "new binary");
        write(
            &staged.path().join("build.json"),
            "{\"platform\":\"linux\"}",
        );
        write(&staged.path().join("nested/stillus.svg"), "icon");
        installation.apply(&staged).unwrap();

        assert_eq!(
            fs::read_to_string(root.join("stillus")).unwrap(),
            "new binary"
        );
        assert_eq!(
            fs::read_to_string(root.join("nested/stillus.svg")).unwrap(),
            "icon"
        );
        // Files the package does not carry are left untouched.
        assert_eq!(
            fs::read_to_string(root.join("LICENSE.txt")).unwrap(),
            "old license"
        );
        drop(staged);
        installation.cleanup();
        let leftovers = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(STAGING_PREFIX))
            })
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn staging_directories_disappear_with_the_guard() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("linux");
        write(&root.join("stillus"), "binary");
        write(&root.join("build.json"), "{}");
        let installation = Installation::linux(root.join("stillus")).unwrap();
        let path = {
            let staging = installation.staging().unwrap();
            let path = staging.path().to_path_buf();
            assert!(path.is_dir());
            path
        };
        assert!(!path.exists());
    }
}
