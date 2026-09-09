// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! Application updates from the project's GitHub releases.
//!
//! The crate reads release metadata from a fixed repository, verifies every
//! downloaded byte against the release checksum list and replaces the installed
//! application in place. It never reads notes, and never writes anywhere inside
//! a workspace.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

mod archive;
mod github;
mod install;
mod payload;
mod transport;

pub use archive::ArchiveKind;
pub use install::{InstallKind, Installation};
pub use transport::{HttpsTransport, UpdateTransport};

/// Repository that publishes Stillus releases.
pub const REPOSITORY: &str = "stillus-ai/stillus";
/// Release asset that lists the SHA-256 digest of every other asset.
pub const CHECKSUMS: &str = "SHA256SUMS";
/// Automatic checks ignore releases published less than this long ago.
pub const AUTOMATIC_HOLD_MS: i64 = 24 * 60 * 60 * 1000;

const MAX_METADATA_BYTES: u64 = 1024 * 1024;
const MAX_CHECKSUM_BYTES: u64 = 64 * 1024;
const MAX_PACKAGE_BYTES: u64 = 400 * 1024 * 1024;

/// Every failure mode is deliberately coarse: server text never reaches the
/// user interface, only local paths and IO errors do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateError {
    /// The running program is not a packaged installation.
    NotInstalled,
    /// The installation directory cannot be written by this user.
    ReadOnly,
    /// The release could not be reached.
    Network,
    /// GitHub refused the request, normally an anonymous rate limit.
    RateLimited,
    /// The response was not the expected release metadata.
    Response,
    /// The release has no package for this platform.
    NoPackage,
    /// A downloaded file did not match the published checksum.
    Checksum,
    /// The package contents were rejected.
    Package(&'static str),
    /// A local filesystem operation failed.
    Io(String),
}

impl fmt::Display for UpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInstalled => formatter.write_str("this build is not an installed package"),
            Self::ReadOnly => formatter.write_str("the installation directory is not writable"),
            Self::Network => formatter.write_str("the release service could not be reached"),
            Self::RateLimited => formatter.write_str("the release service refused the request"),
            Self::Response => formatter.write_str("unexpected release metadata"),
            Self::NoPackage => formatter.write_str("the release has no package for this platform"),
            Self::Checksum => formatter.write_str("checksum mismatch"),
            Self::Package(detail) => write!(formatter, "rejected package: {detail}"),
            Self::Io(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for UpdateError {}

impl From<std::io::Error> for UpdateError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

/// A `major.minor.patch` application version.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Parses `1.2.3` or `v1.2.3`. Leading zeros and extra components are
    /// rejected so that a tag can never compare as something it is not.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        let value = value.strip_prefix('v').unwrap_or(value);
        let mut parts = value.split('.');
        let major = component(parts.next()?)?;
        let minor = component(parts.next()?)?;
        let patch = component(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self::new(major, minor, patch))
    }
}

fn component(value: &str) -> Option<u32> {
    if value.is_empty()
        || value.len() > 5
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return None;
    }
    value.parse().ok()
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// A downloadable release file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseAsset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

/// The published release that the `latest` tag points at.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Release {
    pub version: Version,
    pub tag: String,
    pub published_at_ms: i64,
    pub page_url: String,
    pub notes: String,
    pub assets: Vec<ReleaseAsset>,
}

impl Release {
    pub fn asset(&self, name: &str) -> Option<&ReleaseAsset> {
        self.assets.iter().find(|asset| asset.name == name)
    }

    /// Milliseconds since publication, never negative for a skewed clock.
    pub fn age_ms(&self, now_ms: i64) -> i64 {
        now_ms.saturating_sub(self.published_at_ms).max(0)
    }
}

/// Manual checks install any newer release; automatic checks hold back
/// releases that are younger than [`AUTOMATIC_HOLD_MS`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckMode {
    Automatic,
    Manual,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Decision {
    /// Nothing newer than the running version is published.
    UpToDate,
    /// A newer release with a package for this platform.
    Available(Release),
    /// A newer release that automatic checks still hold back.
    Held { release: Release, remaining_ms: i64 },
    /// A newer release without a usable package for this platform.
    Unpackaged(Release),
}

impl Decision {
    pub fn release(&self) -> Option<&Release> {
        match self {
            Self::UpToDate => None,
            Self::Available(release) | Self::Held { release, .. } | Self::Unpackaged(release) => {
                Some(release)
            }
        }
    }
}

/// Wall-clock milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
        Err(error) => -i64::try_from(error.duration().as_millis()).unwrap_or(i64::MAX),
    }
}

/// Package file name published for the running platform.
pub fn package_name(version: Version) -> Option<String> {
    package_name_for(version, std::env::consts::OS, std::env::consts::ARCH)
}

fn package_name_for(version: Version, os: &str, arch: &str) -> Option<String> {
    let (platform, arch) = match (os, arch) {
        ("macos", "aarch64") => ("macos", "arm64"),
        ("windows", "x86_64") => ("windows", "x86_64"),
        ("linux", "x86_64") => ("linux", "x86_64"),
        ("linux", "aarch64") => ("linux", "aarch64"),
        _ => return None,
    };
    let extension = if platform == "windows" {
        "zip"
    } else {
        "tar.gz"
    };
    Some(format!("stillus-{version}-{platform}-{arch}.{extension}"))
}

/// Applies the update policy to a published release.
pub fn evaluate(current: Version, release: Release, now_ms: i64, mode: CheckMode) -> Decision {
    if release.version <= current {
        return Decision::UpToDate;
    }
    if mode == CheckMode::Automatic {
        let age = release.age_ms(now_ms);
        if age < AUTOMATIC_HOLD_MS {
            return Decision::Held {
                remaining_ms: AUTOMATIC_HOLD_MS - age,
                release,
            };
        }
    }
    let packaged = package_name(release.version)
        .is_some_and(|name| release.asset(&name).is_some() && release.asset(CHECKSUMS).is_some());
    if packaged {
        Decision::Available(release)
    } else {
        Decision::Unpackaged(release)
    }
}

/// Reads the release that GitHub currently marks as the latest one.
pub fn check(
    transport: &dyn UpdateTransport,
    current: Version,
    mode: CheckMode,
    now_ms: i64,
) -> Result<Decision, UpdateError> {
    let url = format!("https://api.github.com/repos/{REPOSITORY}/releases/latest");
    let body = transport.fetch(
        &url,
        "application/vnd.github+json",
        MAX_METADATA_BYTES,
        &mut |_, _| {},
    )?;
    let release = github::release(&body)?;
    Ok(evaluate(current, release, now_ms, mode))
}

/// Downloads, verifies and installs a release in place.
///
/// The caller offers an explicit restart afterwards; installation itself
/// never starts a process or closes the running application.
pub fn install(
    transport: &dyn UpdateTransport,
    installation: &Installation,
    release: &Release,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<(), UpdateError> {
    let name = package_name(release.version).ok_or(UpdateError::NoPackage)?;
    let package = release.asset(&name).ok_or(UpdateError::NoPackage)?;
    let checksums = release.asset(CHECKSUMS).ok_or(UpdateError::NoPackage)?;
    let kind = ArchiveKind::for_name(&name).ok_or(UpdateError::NoPackage)?;
    installation.ensure_writable()?;
    let list = transport.fetch(
        &checksums.url,
        "application/octet-stream",
        MAX_CHECKSUM_BYTES,
        &mut |_, _| {},
    )?;
    let expected = checksum(&list, &name).ok_or(UpdateError::Checksum)?;
    let bytes = transport.fetch(
        &package.url,
        "application/octet-stream",
        MAX_PACKAGE_BYTES,
        progress,
    )?;
    if digest(&bytes) != expected {
        return Err(UpdateError::Checksum);
    }
    let staging = installation.staging()?;
    archive::extract(&bytes, kind, staging.path())?;
    payload::validate(staging.path(), installation, release.version)?;
    installation.apply(staging.path())?;
    drop(staging);
    installation.cleanup();
    Ok(())
}

/// Hexadecimal SHA-256 digest.
pub(crate) fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let mut result = String::with_capacity(64);
    for byte in hasher.finalize() {
        result.push_str(&format!("{byte:02x}"));
    }
    result
}

/// Reads one entry out of a `sha256sum` style list.
pub(crate) fn checksum(list: &[u8], name: &str) -> Option<String> {
    let text = std::str::from_utf8(list).ok()?;
    if text.len() > MAX_CHECKSUM_BYTES as usize {
        return None;
    }
    let mut found = None;
    for line in text.lines() {
        let Some((digest, file)) = line.split_once("  ") else {
            continue;
        };
        let digest = digest.trim();
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        if file.trim() != name {
            continue;
        }
        if found.is_some() {
            // A duplicated entry is ambiguous; refuse rather than guess.
            return None;
        }
        found = Some(digest.to_ascii_lowercase());
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(version: &str, published_at_ms: i64, assets: &[&str]) -> Release {
        Release {
            version: Version::parse(version).unwrap(),
            tag: format!("v{version}"),
            published_at_ms,
            page_url: format!("https://github.com/{REPOSITORY}/releases/tag/v{version}"),
            notes: String::new(),
            assets: assets
                .iter()
                .map(|name| ReleaseAsset {
                    name: (*name).to_owned(),
                    url: format!(
                        "https://github.com/{REPOSITORY}/releases/download/v{version}/{name}"
                    ),
                    size: 1,
                })
                .collect(),
        }
    }

    fn packaged(version: &str, published_at_ms: i64) -> Release {
        let package = package_name(Version::parse(version).unwrap()).unwrap();
        release(version, published_at_ms, &[package.as_str(), CHECKSUMS])
    }

    #[test]
    fn versions_parse_and_order() {
        assert_eq!(Version::parse("v0.1.2"), Some(Version::new(0, 1, 2)));
        assert_eq!(Version::parse(" 1.10.0 "), Some(Version::new(1, 10, 0)));
        assert_eq!(Version::parse("0.1.0").unwrap().to_string(), "0.1.0");
        assert!(Version::parse("1.2").is_none());
        assert!(Version::parse("1.2.3.4").is_none());
        assert!(Version::parse("1.2.03").is_none());
        assert!(Version::parse("1.2.x").is_none());
        assert!(Version::parse("1.2.3-rc1").is_none());
        assert!(Version::parse("1.2.123456").is_none());
        assert!(Version::new(0, 1, 10) > Version::new(0, 1, 9));
        assert!(Version::new(0, 2, 0) > Version::new(0, 1, 99));
    }

    #[test]
    fn automatic_checks_hold_releases_for_a_day() {
        let current = Version::new(0, 1, 0);
        let now = 10 * AUTOMATIC_HOLD_MS;
        let fresh = packaged("0.1.1", now - AUTOMATIC_HOLD_MS + 1);
        assert!(matches!(
            evaluate(current, fresh.clone(), now, CheckMode::Automatic),
            Decision::Held {
                remaining_ms: 1,
                ..
            }
        ));
        assert!(matches!(
            evaluate(current, fresh, now, CheckMode::Manual),
            Decision::Available(_)
        ));
        let ripe = packaged("0.1.1", now - AUTOMATIC_HOLD_MS);
        assert!(matches!(
            evaluate(current, ripe, now, CheckMode::Automatic),
            Decision::Available(_)
        ));
    }

    #[test]
    fn only_newer_packaged_releases_install() {
        let current = Version::new(0, 2, 0);
        let now = 10 * AUTOMATIC_HOLD_MS;
        for tag in ["0.2.0", "0.1.9"] {
            assert_eq!(
                evaluate(current, packaged(tag, 0), now, CheckMode::Manual),
                Decision::UpToDate
            );
        }
        assert!(matches!(
            evaluate(
                current,
                release("0.2.1", 0, &[CHECKSUMS]),
                now,
                CheckMode::Manual
            ),
            Decision::Unpackaged(_)
        ));
        let package = package_name(Version::new(0, 2, 1)).unwrap();
        assert!(matches!(
            evaluate(
                current,
                release("0.2.1", 0, &[package.as_str()]),
                now,
                CheckMode::Manual
            ),
            Decision::Unpackaged(_)
        ));
        assert!(matches!(
            evaluate(current, packaged("0.2.1", 0), now, CheckMode::Manual),
            Decision::Available(_)
        ));
    }

    #[test]
    fn a_release_published_in_the_future_is_still_held() {
        let now = 10 * AUTOMATIC_HOLD_MS;
        let decision = evaluate(
            Version::new(0, 1, 0),
            packaged("0.1.1", now + AUTOMATIC_HOLD_MS),
            now,
            CheckMode::Automatic,
        );
        assert!(matches!(
            decision,
            Decision::Held {
                remaining_ms: AUTOMATIC_HOLD_MS,
                ..
            }
        ));
    }

    #[test]
    fn package_names_follow_the_publisher() {
        let version = Version::new(1, 2, 3);
        assert_eq!(
            package_name_for(version, "macos", "aarch64").as_deref(),
            Some("stillus-1.2.3-macos-arm64.tar.gz")
        );
        assert_eq!(
            package_name_for(version, "windows", "x86_64").as_deref(),
            Some("stillus-1.2.3-windows-x86_64.zip")
        );
        assert_eq!(
            package_name_for(version, "linux", "aarch64").as_deref(),
            Some("stillus-1.2.3-linux-aarch64.tar.gz")
        );
        assert_eq!(package_name_for(version, "macos", "x86_64"), None);
        assert_eq!(package_name_for(version, "freebsd", "x86_64"), None);
    }

    #[test]
    fn checksum_lists_are_read_strictly() {
        let list = concat!(
            "0000000000000000000000000000000000000000000000000000000000000001  stillus-1.2.3-linux-x86_64.tar.gz\n",
            "0000000000000000000000000000000000000000000000000000000000000002  SHA256SUMS\n",
            "not-a-digest  stillus-1.2.3-macos-arm64.tar.gz\n",
        );
        assert_eq!(
            checksum(list.as_bytes(), "SHA256SUMS").as_deref(),
            Some("0000000000000000000000000000000000000000000000000000000000000002")
        );
        assert_eq!(
            checksum(list.as_bytes(), "stillus-1.2.3-macos-arm64.tar.gz"),
            None
        );
        assert_eq!(checksum(list.as_bytes(), "missing"), None);
        let duplicated = concat!(
            "0000000000000000000000000000000000000000000000000000000000000001  same\n",
            "0000000000000000000000000000000000000000000000000000000000000002  same\n",
        );
        assert_eq!(checksum(duplicated.as_bytes(), "same"), None);
        assert!(checksum(b"\xff\xfe", "same").is_none());
    }

    #[test]
    fn digests_are_lowercase_hexadecimal() {
        assert_eq!(
            digest(b"stillus"),
            "6d9fb33811eb5894300ff392a5854d61f11658aa04a7f5eb2c18190b8e5688aa"
        );
    }
}
