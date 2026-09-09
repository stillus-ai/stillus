// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! Checks an extracted package before it replaces the installed application.
//!
//! The published checksum list already covers the archive as a whole. This is
//! the second half of the same guarantee: the archive must be the package the
//! publisher describes, for this platform and for the version that was
//! offered, with every file matching the manifest the packager wrote.

use crate::{InstallKind, Installation, UpdateError, Version, digest};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const BUILD_MANIFEST: &str = "build.json";
const SOURCE_REVISION: &str = "SOURCE_REVISION.txt";
const MAC_MANIFEST: &str = "Stillus.app/Contents/Resources/release.json";
const MAC_EXECUTABLE: &str = "Stillus.app/Contents/MacOS/Stillus";
const LINUX_EXECUTABLE: &str = "stillus";
const WINDOWS_EXECUTABLE: &str = "Stillus.exe";

#[derive(Deserialize)]
struct BuildManifest {
    source_revision: String,
    platform: String,
    files: Vec<BuildFile>,
}

#[derive(Deserialize)]
struct BuildFile {
    path: String,
    sha256: String,
}

#[derive(Deserialize)]
struct BundleManifest {
    version: String,
    bundle_identifier: String,
}

pub(crate) fn validate(
    root: &Path,
    installation: &Installation,
    version: Version,
) -> Result<(), UpdateError> {
    let manifest: BuildManifest = read_json(&root.join(BUILD_MANIFEST))?;
    let kind = installation.kind();
    if !manifest.platform.starts_with(kind.platform()) {
        return Err(UpdateError::Package("package is for another platform"));
    }
    let revision = fs::read_to_string(root.join(SOURCE_REVISION))
        .map_err(|_| UpdateError::Package("package has no source revision"))?;
    if revision.trim() != manifest.source_revision || manifest.source_revision.is_empty() {
        return Err(UpdateError::Package("package source revision disagrees"));
    }
    let mut listed = BTreeSet::new();
    for file in &manifest.files {
        let relative = relative(&file.path)?;
        let bytes = fs::read(root.join(&relative))
            .map_err(|_| UpdateError::Package("package is missing a listed file"))?;
        if digest(&bytes) != file.sha256.to_ascii_lowercase() {
            return Err(UpdateError::Package("package file checksum disagrees"));
        }
        listed.insert(relative);
    }
    // The manifest never lists itself; anything else unlisted is unexpected.
    listed.insert(PathBuf::from(BUILD_MANIFEST));
    let mut present = Vec::new();
    files(root, Path::new(""), &mut present)?;
    if present.iter().any(|path| !listed.contains(path)) {
        return Err(UpdateError::Package("package carries unlisted files"));
    }
    match kind {
        InstallKind::MacApp => {
            let bundle: BundleManifest = read_json(&root.join(MAC_MANIFEST))?;
            if bundle.bundle_identifier != "org.stillus.Stillus" {
                return Err(UpdateError::Package("package is another application"));
            }
            if Version::parse(&bundle.version) != Some(version) {
                return Err(UpdateError::Package("package version disagrees"));
            }
            require(root, MAC_EXECUTABLE)?;
        }
        InstallKind::Linux => require(root, LINUX_EXECUTABLE)?,
        InstallKind::Windows => require(root, WINDOWS_EXECUTABLE)?,
    }
    Ok(())
}

fn require(root: &Path, relative: &str) -> Result<(), UpdateError> {
    if root.join(relative).is_file() {
        Ok(())
    } else {
        Err(UpdateError::Package(
            "package has no application executable",
        ))
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, UpdateError> {
    let bytes =
        fs::read(path).map_err(|_| UpdateError::Package("package has no build manifest"))?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(UpdateError::Package("package manifest is too large"));
    }
    serde_json::from_slice(&bytes).map_err(|_| UpdateError::Package("package manifest is invalid"))
}

/// Manifest paths are POSIX and relative; extraction already rejected every
/// disallowed archive path, so this only has to agree with what reached disk.
fn relative(path: &str) -> Result<PathBuf, UpdateError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.split('/').any(|part| part.is_empty() || part == "..")
    {
        return Err(UpdateError::Package("disallowed manifest path"));
    }
    crate::archive::relative(path, 0)
        .map_err(|_| UpdateError::Package("disallowed manifest path"))?
        .ok_or(UpdateError::Package("disallowed manifest path"))
}

fn files(root: &Path, prefix: &Path, result: &mut Vec<PathBuf>) -> Result<(), UpdateError> {
    for entry in fs::read_dir(root.join(prefix))? {
        let entry = entry?;
        let relative = prefix.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            files(root, &relative, result)?;
        } else {
            result.push(relative);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        directory: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                directory: tempfile::tempdir().unwrap(),
            }
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.directory.path().join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, contents).unwrap();
        }

        fn path(&self) -> &Path {
            self.directory.path()
        }
    }

    fn manifest(platform: &str, files: &[(&str, &str)]) -> String {
        let entries = files
            .iter()
            .map(|(path, contents)| {
                format!(
                    "{{\"path\": \"{path}\", \"sha256\": \"{}\"}}",
                    digest(contents.as_bytes())
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{{\"source_revision\": \"abc123\", \"platform\": \"{platform}\", \"architecture\": \"test\", \"files\": [{entries}]}}"
        )
    }

    fn linux_package(fixture: &Fixture, revision: &str) -> Installation {
        let install = fixture.path().join("install");
        fs::create_dir_all(&install).unwrap();
        fs::write(install.join("stillus"), "installed").unwrap();
        fs::write(install.join("build.json"), "{}").unwrap();
        fixture.write("staged/stillus", "binary");
        fixture.write("staged/SOURCE_REVISION.txt", revision);
        fixture.write(
            "staged/build.json",
            &manifest(
                "linux",
                &[("stillus", "binary"), ("SOURCE_REVISION.txt", revision)],
            ),
        );
        Installation::linux(install.join("stillus")).unwrap()
    }

    #[test]
    fn a_matching_linux_package_is_accepted() {
        let fixture = Fixture::new();
        let installation = linux_package(&fixture, "abc123\n");
        validate(
            &fixture.path().join("staged"),
            &installation,
            Version::new(1, 0, 0),
        )
        .unwrap();
    }

    #[test]
    fn tampered_packages_are_refused() {
        let fixture = Fixture::new();
        let installation = linux_package(&fixture, "abc123\n");
        let staged = fixture.path().join("staged");

        fixture.write("staged/stillus", "tampered");
        assert_eq!(
            validate(&staged, &installation, Version::new(1, 0, 0)),
            Err(UpdateError::Package("package file checksum disagrees"))
        );

        fixture.write("staged/stillus", "binary");
        fixture.write("staged/extra.txt", "unexpected");
        assert_eq!(
            validate(&staged, &installation, Version::new(1, 0, 0)),
            Err(UpdateError::Package("package carries unlisted files"))
        );
        fs::remove_file(staged.join("extra.txt")).unwrap();

        fixture.write("staged/SOURCE_REVISION.txt", "def456\n");
        assert_eq!(
            validate(&staged, &installation, Version::new(1, 0, 0)),
            Err(UpdateError::Package("package source revision disagrees"))
        );
    }

    #[test]
    fn a_package_for_another_platform_is_refused() {
        let fixture = Fixture::new();
        let installation = linux_package(&fixture, "abc123\n");
        fixture.write(
            "staged/build.json",
            &manifest(
                "windows",
                &[("stillus", "binary"), ("SOURCE_REVISION.txt", "abc123\n")],
            ),
        );
        assert_eq!(
            validate(
                &fixture.path().join("staged"),
                &installation,
                Version::new(1, 0, 0)
            ),
            Err(UpdateError::Package("package is for another platform"))
        );
    }

    #[test]
    fn a_bundle_must_carry_the_offered_version() {
        let fixture = Fixture::new();
        let bundle = fixture.path().join("install/Stillus.app");
        fixture.write("install/Stillus.app/Contents/MacOS/Stillus", "installed");
        fixture.write("install/Stillus.app/Contents/Resources/release.json", "{}");
        let installation = Installation::mac_app(bundle).unwrap();

        let executable = "Stillus.app/Contents/MacOS/Stillus";
        let release = "Stillus.app/Contents/Resources/release.json";
        let bundle_manifest =
            "{\"version\": \"1.2.3\", \"bundle_identifier\": \"org.stillus.Stillus\"}";
        fixture.write(&format!("staged/{executable}"), "binary");
        fixture.write(&format!("staged/{release}"), bundle_manifest);
        fixture.write("staged/SOURCE_REVISION.txt", "abc123\n");
        fixture.write(
            "staged/build.json",
            &manifest(
                "macos",
                &[
                    (executable, "binary"),
                    (release, bundle_manifest),
                    ("SOURCE_REVISION.txt", "abc123\n"),
                ],
            ),
        );
        let staged = fixture.path().join("staged");
        validate(&staged, &installation, Version::new(1, 2, 3)).unwrap();
        assert_eq!(
            validate(&staged, &installation, Version::new(1, 2, 4)),
            Err(UpdateError::Package("package version disagrees"))
        );
    }

    #[test]
    fn manifest_paths_stay_inside_the_package() {
        assert_eq!(relative("a/b.txt").unwrap(), PathBuf::from("a/b.txt"));
        for rejected in [
            "",
            "/etc/passwd",
            "a//b",
            "a/../b",
            "a\\b",
            "C:/outside",
            "C:outside",
            "\\\\server\\share",
            "a/..\\outside",
            "a:stream",
            ".",
        ] {
            assert!(relative(rejected).is_err(), "{rejected}");
        }
    }

    #[test]
    fn unsafe_manifest_paths_are_rejected_before_reading_files() {
        for path in [
            "a\\b",
            "a/..\\outside",
            "C:/outside",
            "\\\\server\\share",
            "a/../outside",
        ] {
            let fixture = Fixture::new();
            let installation = linux_package(&fixture, "abc123\n");
            fixture.write(
                "staged/build.json",
                &serde_json::json!({
                    "platform": "linux", "source_revision": "abc123",
                    "files": [{"path": path, "sha256": digest(b"untrusted")}]
                })
                .to_string(),
            );
            assert_eq!(
                validate(
                    &fixture.path().join("staged"),
                    &installation,
                    Version::new(1, 0, 0)
                ),
                Err(UpdateError::Package("disallowed manifest path"))
            );
        }
    }
}
