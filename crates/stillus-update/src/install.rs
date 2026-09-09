// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! Locating the installed application and replacing it in place.
//!
//! Replacement is a sequence of renames inside the installation directory, so
//! a failure at any point can be rolled back and the running program keeps the
//! files it already opened. The caller offers an explicit restart afterwards;
//! installation never starts a process or closes the running application.

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
        let path = unique(&self.root, "staging");
        fs::create_dir(&path)?;
        Ok(Staging { path })
    }

    /// Moves an extracted package into place.
    pub(crate) fn apply(&self, staged: &Path) -> Result<(), UpdateError> {
        match self.kind {
            InstallKind::MacApp => self.apply_bundle(staged),
            InstallKind::Linux | InstallKind::Windows => self.apply_files(staged),
        }
    }

    fn apply_bundle(&self, staged: &Path) -> Result<(), UpdateError> {
        let source = staged.join(BUNDLE_NAME);
        if !source.join(MAC_EXECUTABLE).is_file() {
            return Err(UpdateError::Package("package has no application bundle"));
        }
        let retired = unique(&self.root, "bundle");
        fs::rename(&self.target, &retired)?;
        if let Err(error) = fs::rename(&source, &self.target) {
            let _ = fs::rename(&retired, &self.target);
            return Err(UpdateError::Io(error.to_string()));
        }
        let _ = fs::remove_dir_all(&retired);
        Ok(())
    }

    fn apply_files(&self, staged: &Path) -> Result<(), UpdateError> {
        let mut relative = Vec::new();
        collect(staged, Path::new(""), &mut relative)?;
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
    /// at any time; failures are ignored because the next start retries.
    pub fn cleanup(&self) {
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

    #[test]
    fn bundles_are_replaced_and_restored_on_failure() {
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
        installation.apply(staged.path()).unwrap();
        assert_eq!(
            fs::read_to_string(bundle.join(MAC_EXECUTABLE)).unwrap(),
            "new binary"
        );
        drop(staged);

        let empty = installation.staging().unwrap();
        assert_eq!(
            installation.apply(empty.path()),
            Err(UpdateError::Package("package has no application bundle"))
        );
        assert_eq!(
            fs::read_to_string(bundle.join(MAC_EXECUTABLE)).unwrap(),
            "new binary"
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
        installation.apply(staged.path()).unwrap();

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
