// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]
#![cfg(test)]

//! Drives a complete update against a package built in the test, without a
//! network: metadata, checksum list and archive all come from one fixture.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use stillus_update::{
    CheckMode, Decision, Installation, UpdateError, UpdateTransport, Version, check, install,
    package_name,
};

const CURRENT: Version = Version::new(0, 1, 0);
const NEXT: Version = Version::new(0, 1, 1);
const REVISION: &str = "0123456789abcdef0123456789abcdef01234567";
const API: &str = "https://api.github.com/repos/stillus-ai/stillus/releases/latest";

#[derive(Default)]
struct Fixture {
    responses: BTreeMap<String, Vec<u8>>,
    requests: Mutex<Vec<String>>,
}

impl UpdateTransport for Fixture {
    fn fetch(
        &self,
        url: &str,
        _accept: &str,
        limit: u64,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<Vec<u8>, UpdateError> {
        self.requests.lock().unwrap().push(url.to_owned());
        let body = self
            .responses
            .get(url)
            .cloned()
            .ok_or(UpdateError::Response)?;
        if body.len() as u64 > limit {
            return Err(UpdateError::Response);
        }
        progress(body.len() as u64, Some(body.len() as u64));
        Ok(body)
    }
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Files of a package for the platform the test runs on, as relative paths.
fn package_files() -> Vec<(String, Vec<u8>)> {
    let executable = if cfg!(target_os = "macos") {
        "Stillus.app/Contents/MacOS/Stillus"
    } else if cfg!(windows) {
        "Stillus.exe"
    } else {
        "stillus"
    };
    let mut files = vec![(executable.to_owned(), b"new executable".to_vec())];
    if cfg!(target_os = "macos") {
        let manifest =
            format!("{{\"version\": \"{NEXT}\", \"bundle_identifier\": \"org.stillus.Stillus\"}}");
        files.push((
            "Stillus.app/Contents/Resources/release.json".to_owned(),
            manifest.into_bytes(),
        ));
    }
    if cfg!(windows) {
        files.push(("dependencies.json".to_owned(), b"{}".to_vec()));
    }
    files.push((
        "SOURCE_REVISION.txt".to_owned(),
        format!("{REVISION}\n").into_bytes(),
    ));
    let listed = files
        .iter()
        .map(|(path, data)| format!("{{\"path\": \"{path}\", \"sha256\": \"{}\"}}", sha256(data)))
        .collect::<Vec<_>>()
        .join(", ");
    let platform = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        "linux"
    };
    let manifest = format!(
        "{{\"source_revision\": \"{REVISION}\", \"platform\": \"{platform}\", \"architecture\": \"test\", \"files\": [{listed}]}}"
    );
    files.push(("build.json".to_owned(), manifest.into_bytes()));
    files
}

fn package(name: &str) -> Vec<u8> {
    let files = package_files();
    if name.ends_with(".zip") {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (path, data) in files {
            writer
                .start_file(path, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(&data).unwrap();
        }
        return writer.finish().unwrap().into_inner();
    }
    let root = name.trim_end_matches(".tar.gz");
    let mut builder = tar::Builder::new(Vec::new());
    for (path, data) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(if path.ends_with("Stillus") || path.ends_with("stillus") {
            0o755
        } else {
            0o644
        });
        header.set_cksum();
        builder
            .append_data(&mut header, format!("{root}/{path}"), data.as_slice())
            .unwrap();
    }
    let raw = builder.into_inner().unwrap();
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&raw).unwrap();
    encoder.finish().unwrap()
}

fn metadata(published_at: &str, name: &str) -> Vec<u8> {
    let download = format!("https://github.com/stillus-ai/stillus/releases/download/v{NEXT}");
    format!(
        "{{\"tag_name\": \"v{NEXT}\", \"draft\": false, \"prerelease\": false,
          \"published_at\": \"{published_at}\",
          \"html_url\": \"https://github.com/stillus-ai/stillus/releases/tag/v{NEXT}\",
          \"body\": \"Improvements\\n\\nNone.\",
          \"assets\": [
            {{\"name\": \"{name}\", \"size\": 1, \"state\": \"uploaded\",
              \"browser_download_url\": \"{download}/{name}\"}},
            {{\"name\": \"SHA256SUMS\", \"size\": 1, \"state\": \"uploaded\",
              \"browser_download_url\": \"{download}/SHA256SUMS\"}}
          ]}}"
    )
    .into_bytes()
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

/// An installed application of the running platform's shape.
fn installed(root: &Path) -> (Installation, PathBuf) {
    if cfg!(target_os = "macos") {
        let bundle = root.join("Stillus.app");
        let executable = bundle.join("Contents/MacOS/Stillus");
        write(&executable, "old executable");
        write(&bundle.join("Contents/Resources/release.json"), "{}");
        (Installation::mac_app(bundle).unwrap(), executable)
    } else if cfg!(windows) {
        let executable = root.join("Stillus.exe");
        write(&executable, "old executable");
        write(&root.join("dependencies.json"), "{}");
        (
            Installation::windows(executable.clone()).unwrap(),
            executable,
        )
    } else {
        let executable = root.join("stillus");
        write(&executable, "old executable");
        write(&root.join("build.json"), "{}");
        (Installation::linux(executable.clone()).unwrap(), executable)
    }
}

fn fixture(published_at: &str, corrupt: bool) -> (Fixture, String) {
    let name = package_name(NEXT).expect("the test platform publishes packages");
    let archive = package(&name);
    let digest = if corrupt {
        "0".repeat(64)
    } else {
        sha256(&archive)
    };
    let download = format!("https://github.com/stillus-ai/stillus/releases/download/v{NEXT}");
    let mut responses = BTreeMap::new();
    responses.insert(API.to_owned(), metadata(published_at, &name));
    responses.insert(
        format!("{download}/SHA256SUMS"),
        format!("{digest}  {name}\n").into_bytes(),
    );
    responses.insert(format!("{download}/{name}"), archive);
    (
        Fixture {
            responses,
            requests: Mutex::new(Vec::new()),
        },
        name,
    )
}

#[test]
fn a_published_release_is_downloaded_verified_and_installed() {
    let directory = tempfile::tempdir().unwrap();
    let (installation, executable) = installed(directory.path());
    let (transport, name) = fixture("2020-01-01T00:00:00Z", false);
    let now = stillus_update::now_ms();

    let decision = check(&transport, CURRENT, CheckMode::Automatic, now).unwrap();
    let Decision::Available(release) = decision else {
        panic!("a release published years ago must be offered: {decision:?}");
    };
    assert_eq!(release.version, NEXT);
    assert!(release.asset(&name).is_some());

    let mut seen = Vec::new();
    install(&transport, &installation, &release, &mut |read, total| {
        seen.push((read, total))
    })
    .unwrap();
    assert!(!seen.is_empty());
    assert_eq!(fs::read_to_string(&executable).unwrap(), "new executable");
    // Nothing is left behind in the installation directory.
    installation.cleanup();
    let hidden = fs::read_dir(installation.root())
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with('.'))
        .count();
    assert_eq!(hidden, 0);
}

#[test]
fn a_release_that_fails_verification_never_reaches_the_installation() {
    let directory = tempfile::tempdir().unwrap();
    let (installation, executable) = installed(directory.path());
    let (transport, _) = fixture("2020-01-01T00:00:00Z", true);
    let now = stillus_update::now_ms();
    let Decision::Available(release) = check(&transport, CURRENT, CheckMode::Manual, now).unwrap()
    else {
        panic!("the release must be offered");
    };
    assert_eq!(
        install(&transport, &installation, &release, &mut |_, _| {}),
        Err(UpdateError::Checksum)
    );
    assert_eq!(fs::read_to_string(&executable).unwrap(), "old executable");
}

#[test]
fn a_fresh_release_installs_only_on_a_manual_check() {
    let now = stillus_update::now_ms();
    let published = now - 3 * 60 * 60 * 1000;
    let seconds = published / 1000;
    let stamp = rfc3339(seconds);
    let (transport, _) = fixture(&stamp, false);
    assert!(matches!(
        check(&transport, CURRENT, CheckMode::Automatic, now).unwrap(),
        Decision::Held { .. }
    ));
    assert!(matches!(
        check(&transport, CURRENT, CheckMode::Manual, now).unwrap(),
        Decision::Available(_)
    ));
}

#[test]
fn an_unknown_endpoint_is_reported_as_a_response_failure() {
    let transport = Fixture::default();
    assert_eq!(
        check(&transport, CURRENT, CheckMode::Manual, 0),
        Err(UpdateError::Response)
    );
    assert_eq!(
        transport.requests.lock().unwrap().as_slice(),
        &[API.to_owned()]
    );
}

/// Formats a UTC timestamp the way GitHub publishes it.
fn rfc3339(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let time = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3600,
        (time % 3600) / 60,
        time % 60
    )
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}
