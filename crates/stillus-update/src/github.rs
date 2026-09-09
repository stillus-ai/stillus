// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! Parses the release metadata that GitHub serves for the latest release.
//!
//! Everything that reaches the user interface from here is either a version, a
//! URL that belongs to the project's own repository, or release notes with
//! control characters removed.

use crate::{REPOSITORY, Release, ReleaseAsset, UpdateError, Version};
use serde::Deserialize;

const MAX_ASSETS: usize = 32;
const MAX_NOTES_CHARS: usize = 4000;
const MAX_NAME_BYTES: usize = 100;
const MAX_URL_BYTES: usize = 512;
const MAX_TAG_BYTES: usize = 32;

#[derive(Deserialize)]
struct RawRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    published_at: Option<String>,
    html_url: Option<String>,
    body: Option<String>,
    #[serde(default)]
    assets: Vec<RawAsset>,
}

#[derive(Deserialize)]
struct RawAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
    state: Option<String>,
}

pub(crate) fn release(body: &[u8]) -> Result<Release, UpdateError> {
    let raw: RawRelease = serde_json::from_slice(body).map_err(|_| UpdateError::Response)?;
    if raw.draft || raw.prerelease || raw.tag_name.len() > MAX_TAG_BYTES {
        return Err(UpdateError::Response);
    }
    let version = Version::parse(&raw.tag_name).ok_or(UpdateError::Response)?;
    let published_at_ms = raw
        .published_at
        .as_deref()
        .and_then(timestamp_ms)
        .ok_or(UpdateError::Response)?;
    let page_url = raw
        .html_url
        .filter(|url| belongs_to_repository(url, "releases/"))
        .ok_or(UpdateError::Response)?;
    let notes = notes(raw.body.as_deref().unwrap_or_default());
    let mut assets: Vec<ReleaseAsset> = Vec::new();
    for asset in raw.assets.into_iter().take(MAX_ASSETS) {
        if asset.state.as_deref().unwrap_or("uploaded") != "uploaded"
            || !valid_name(&asset.name)
            || !belongs_to_repository(&asset.browser_download_url, "releases/download/")
            || assets.iter().any(|kept| kept.name == asset.name)
        {
            continue;
        }
        assets.push(ReleaseAsset {
            name: asset.name,
            url: asset.browser_download_url,
            size: asset.size,
        });
    }
    Ok(Release {
        version,
        tag: raw.tag_name,
        published_at_ms,
        page_url,
        notes,
        assets,
    })
}

fn belongs_to_repository(url: &str, path: &str) -> bool {
    let prefix = format!("https://github.com/{REPOSITORY}/{path}");
    url.len() <= MAX_URL_BYTES
        && url.starts_with(&prefix)
        && !url[prefix.len()..].contains("..")
        && url
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'\\')
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

/// Release notes are generated text: keep the wording, drop everything that
/// could disturb the layout, and bound the length.
fn notes(body: &str) -> String {
    let mut result = String::new();
    let mut count = 0usize;
    for character in body.replace("\r\n", "\n").chars() {
        if count >= MAX_NOTES_CHARS {
            break;
        }
        if character == '\n' || !character.is_control() {
            result.push(character);
            count += 1;
        }
    }
    result.trim().to_owned()
}

/// Parses the RFC 3339 timestamps GitHub publishes, for example
/// `2026-09-07T12:34:56Z`. Release metadata is always UTC.
fn timestamp_ms(value: &str) -> Option<i64> {
    let value = value.strip_suffix('Z')?;
    let (date, time) = value.split_once('T')?;
    let time = match time.split_once('.') {
        Some((whole, fraction)) => {
            if fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            whole
        }
        None => time,
    };
    let mut date = date.split('-');
    let year = number(date.next()?, 4)?;
    let month = number(date.next()?, 2)?;
    let day = number(date.next()?, 2)?;
    if date.next().is_some() {
        return None;
    }
    let mut time = time.split(':');
    let hour = number(time.next()?, 2)?;
    let minute = number(time.next()?, 2)?;
    let second = number(time.next()?, 2)?;
    if time.next().is_some()
        || !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let seconds = ((days_from_civil(year, month, day) * 24 + hour) * 60 + minute) * 60 + second;
    seconds.checked_mul(1000)
}

fn number(value: &str, width: usize) -> Option<i64> {
    if value.len() != width || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days between 1970-01-01 and a civil date, from Howard Hinnant's
/// `days_from_civil` algorithm.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = r###"{
        "tag_name": "v0.1.2",
        "draft": false,
        "prerelease": false,
        "published_at": "2026-09-07T12:34:56Z",
        "html_url": "https://github.com/stillus-ai/stillus/releases/tag/v0.1.2",
        "body": "## Improvements\n\nFaster search.",
        "assets": [
            {"name": "stillus-0.1.2-macos-arm64.tar.gz", "size": 12,
             "browser_download_url": "https://github.com/stillus-ai/stillus/releases/download/v0.1.2/stillus-0.1.2-macos-arm64.tar.gz",
             "state": "uploaded"},
            {"name": "SHA256SUMS", "size": 3,
             "browser_download_url": "https://github.com/stillus-ai/stillus/releases/download/v0.1.2/SHA256SUMS",
             "state": "uploaded"},
            {"name": "pending.tar.gz", "size": 3,
             "browser_download_url": "https://github.com/stillus-ai/stillus/releases/download/v0.1.2/pending.tar.gz",
             "state": "starter"},
            {"name": "elsewhere.tar.gz", "size": 3,
             "browser_download_url": "https://example.com/elsewhere.tar.gz",
             "state": "uploaded"}
        ]
    }"###;

    #[test]
    fn reads_versions_assets_and_notes() {
        let release = release(BODY.as_bytes()).unwrap();
        assert_eq!(release.version, Version::new(0, 1, 2));
        assert_eq!(release.tag, "v0.1.2");
        assert_eq!(release.published_at_ms, 1_788_784_496_000);
        assert_eq!(release.notes, "## Improvements\n\nFaster search.");
        assert_eq!(release.assets.len(), 2);
        assert!(release.asset("SHA256SUMS").is_some());
        assert!(release.asset("pending.tar.gz").is_none());
        assert!(release.asset("elsewhere.tar.gz").is_none());
    }

    #[test]
    fn drafts_prereleases_and_broken_metadata_are_refused() {
        for (from, to) in [
            ("\"draft\": false", "\"draft\": true"),
            ("\"prerelease\": false", "\"prerelease\": true"),
            ("\"v0.1.2\",", "\"nightly\","),
            ("2026-09-07T12:34:56Z", "2026-13-07T12:34:56Z"),
            ("\"2026-09-07T12:34:56Z\"", "null"),
            (
                "https://github.com/stillus-ai/stillus/releases/tag/v0.1.2",
                "https://example.com/releases/tag/v0.1.2",
            ),
        ] {
            let body = BODY.replace(from, to);
            assert_eq!(
                release(body.as_bytes()),
                Err(UpdateError::Response),
                "{from}"
            );
        }
        assert_eq!(release(b"not json"), Err(UpdateError::Response));
    }

    #[test]
    fn notes_lose_control_characters_and_length() {
        assert_eq!(notes("a\u{7}b\r\nc"), "ab\nc");
        assert_eq!(
            notes(&"x".repeat(MAX_NOTES_CHARS + 10)).len(),
            MAX_NOTES_CHARS
        );
    }

    #[test]
    fn timestamps_match_the_civil_calendar() {
        assert_eq!(timestamp_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(timestamp_ms("2000-02-29T00:00:00Z"), Some(951_782_400_000));
        assert_eq!(
            timestamp_ms("2026-09-07T12:34:56Z"),
            Some(1_788_784_496_000)
        );
        assert_eq!(
            timestamp_ms("2026-09-07T12:34:56.789Z"),
            Some(1_788_784_496_000)
        );
        assert_eq!(timestamp_ms("2027-02-29T00:00:00Z"), None);
        assert_eq!(timestamp_ms("2026-09-07T12:34:56+02:00"), None);
        assert_eq!(timestamp_ms("2026-9-07T12:34:56Z"), None);
        assert_eq!(timestamp_ms("2026-09-07T24:00:00Z"), None);
        assert_eq!(timestamp_ms(""), None);
    }
}
